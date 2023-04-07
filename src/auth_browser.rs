use crate::TokenInfo;
use anyhow::{anyhow, Result};
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EventRequestPaused, FulfillRequestParams,
};
use chromiumoxide::Page;
use futures::StreamExt;
use oauth2::CsrfToken;
use std::borrow::Cow;
use std::ops::Add;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tokio::sync::oneshot;
use url::Url;

#[derive(Error, Debug)]
enum RequestError {
    #[error("No requests with required data. Timeout.")]
    Timeout,
}

const CONTENT_OK: &str = "<html><head></head><body><h1>OK</h1></body></html>";
const CONTENT_NOT_OK: &str = "<html><head></head><body><h1>NOT OK</h1></body></html>";

pub struct AuthBrowser {
    authorization_url: Url,
    callback_url: Url,
}

impl AuthBrowser {
    pub fn new(authorization_url: Url, callback_url: Url) -> Result<AuthBrowser> {
        Ok(AuthBrowser {
            authorization_url,
            callback_url,
        })
    }

    async fn wait_for_first_page(browser: &Browser) -> Result<Page> {
        // TODO: Possible infinite loop if won't be found
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let pages = browser.pages().await?;

            match pages.first() {
                Some(page) => {
                    let first_page_id = page.target_id();

                    return Ok(browser.get_page(first_page_id.to_owned()).await?);
                }
                None => {
                    continue;
                }
            }
        }
    }

    async fn process_request<TResponse, F>(&self, timeout: u64, f: F) -> Result<TResponse>
    where
        TResponse: Send + Clone + Sync + 'static,
        F: Send + Fn(Arc<EventRequestPaused>) -> Option<TResponse> + 'static,
    {
        let (tx_browser, rx_browser) = oneshot::channel();

        log::debug!("Opening chromium instance");
        let (mut browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .with_head()
                .enable_request_intercept()
                .build()
                .map_err(|e| anyhow!(e))?,
        )
        .await?;

        let handle = tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    // TODO: Detect that user has closed a browser. Fail immediately
                    log::error!("Handler created an error");
                    break;
                }
            }
        });

        let page = Arc::new(Self::wait_for_first_page(&browser).await?);

        let mut request_paused = page.event_listener::<EventRequestPaused>().await.unwrap();
        let intercept_page = page.clone();
        let callback_url = self.callback_url.to_owned();
        let intercept_handle = tokio::spawn(async move {
            while let Some(event) = request_paused.next().await {
                let request_url = Url::parse(&event.request.url).unwrap();
                if request_url.origin() == callback_url.origin()
                    && request_url.path() == callback_url.path()
                {
                    log::debug!("Received request to `--callback-url` {}", callback_url);

                    let response = f(event.clone());

                    if let Err(e) = intercept_page
                        .execute(
                            FulfillRequestParams::builder()
                                .request_id(event.request_id.clone())
                                .body(BASE64_STANDARD.encode(if response.is_some() {
                                    CONTENT_OK
                                } else {
                                    CONTENT_NOT_OK
                                }))
                                .response_code(200)
                                .build()
                                .unwrap(),
                        )
                        .await
                    {
                        log::error!("Failed to fullfill request: {e}");
                    }

                    if let Some(response) = response {
                        let _ = tx_browser.send(response);
                        break;
                    }
                } else if let Err(e) = intercept_page
                    .execute(ContinueRequestParams::new(event.request_id.clone()))
                    .await
                {
                    log::error!("Failed to continue request: {e}");
                }
            }
        });

        log::debug!("Opening authorization page {}", self.authorization_url);
        page.goto(self.authorization_url.as_str()).await?;

        let response = tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(timeout)) => {
                log::debug!("Timeout");
                Err::<TResponse, anyhow::Error>(RequestError::Timeout.into())
            }
            Ok(response) = rx_browser => {
                Ok::<TResponse, anyhow::Error>(response)
            }
        };

        browser.close().await?;
        handle.await.unwrap();
        intercept_handle.await.unwrap();

        response
    }

    pub async fn get_code(&self, timeout: u64, csrf_token: CsrfToken) -> Result<String> {
        self.process_request(timeout, move |event| {
            let request_url = Url::parse(&event.request.url).unwrap();
            let state = request_url.query_pairs().find(|qp| qp.0.eq("state"));
            let code = request_url.query_pairs().find(|qp| qp.0.eq("code"));

            match (state, code) {
                (Some((_, state)), Some((_, code))) => {
                    if state == *csrf_token.secret() {
                        let code = code.to_string();
                        log::debug!("Given code: {}", code);

                        Some(code)
                    } else {
                        log::debug!("Incorrect CSRF token. Ignoring...");

                        None
                    }
                }
                _ => {
                    log::debug!(
                        "Call to server without a state and/or a code parameter. Ignoring..."
                    );

                    None
                }
            }
        })
        .await
    }

    pub async fn get_token_data(&self, timeout: u64, csrf_token: CsrfToken) -> Result<TokenInfo> {
        self.process_request(timeout, move |event| match event.request.method.as_str() {
            "POST" => {
                let body = event.request.post_data.as_ref().unwrap();

                log::info!("This is what we get in POST: {:?}", body);
                let form_params =
                    form_urlencoded::parse(body.as_bytes()).collect::<Vec<(Cow<str>, Cow<str>)>>();

                let (_, access_token) = form_params
                    .iter()
                    .find(|(name, _value)| name == "access_token")
                    .expect("Cannot find access_token in the HTTP Post request.");

                let (_, expires_in) = form_params
                    .iter()
                    .find(|(name, _value)| name == "expires_in")
                    .expect("Cannot find expires_in in the HTTP Post request.");

                let (_, state) = form_params
                    .iter()
                    .find(|(name, _value)| name == "state")
                    .expect("Cannot find state in the HTTP Post request.");

                if state == csrf_token.secret() {
                    Some(TokenInfo {
                        access_token: access_token.to_string(),
                        refresh_token: None,
                        expires: Some(
                            SystemTime::now().add(Duration::from_secs(
                                expires_in
                                    .parse::<u64>()
                                    .expect("expires_in is an incorrect number"),
                            )),
                        ),
                        scope: None,
                    })
                } else {
                    log::debug!("Incorrect CSRF token. Aborting...");

                    None
                }
            }
            _ => {
                log::debug!("Call to server without a state and/or a code parameter. Ignoring...");

                None
            }
        })
        .await
    }
}
