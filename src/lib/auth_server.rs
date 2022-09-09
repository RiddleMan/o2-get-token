use crate::TokenInfo;
use oauth2::CsrfToken;
use std::borrow::Cow;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tiny_http::{Header, Method, Request, Response, Server as TinyServer};
use tokio::sync::oneshot;
use url::Url;

#[derive(Debug)]
struct Timeout {}

impl Display for Timeout {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "No requests with required data. Timeout.")
    }
}

impl Error for Timeout {}

pub struct AuthServer {
    server: Arc<TinyServer>,
}

impl AuthServer {
    pub fn new(port: u16) -> AuthServer {
        log::debug!("Creating http server on port {}", port);
        let server = TinyServer::http(format!("127.0.0.1:{}", port)).unwrap();

        log::info!("Waiting for connections...");
        AuthServer {
            server: Arc::new(server),
        }
    }

    fn response_with_default_message(request: Request) -> Result<(), Box<dyn Error>> {
        let html_header = Header::from_str("Content-Type: text/html; charset=UTF-8").unwrap();
        let mut response = Response::from_string("<!doctype html><html lang=\"en\"><script>window.close();</script><head><meta charset=utf-8><title>Doken</title></head><body>Successfully signed in. Close current tab.</body></html>");
        response.add_header(html_header);

        log::debug!("Responding to the user browser..");
        request.respond(response)?;
        Ok(())
    }

    async fn process_request<TResponse, F>(
        &self,
        timeout: u64,
        f: F,
    ) -> Result<TResponse, Box<dyn Error>>
    where
        TResponse: Send + Clone + Sync + 'static,
        F: Send + Fn(Request) -> Option<TResponse> + 'static,
    {
        let (tx_server, rx_server) = oneshot::channel();
        let (tx_sleep, rx_sleep) = oneshot::channel();
        let server = self.server.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout)).await;

            let _ = tx_sleep.send("timeout");
        });

        tokio::spawn(async move {
            for request in server.incoming_requests() {
                log::debug!("Request received");

                match f(request) {
                    Some(response) => {
                        let _ = tx_server.send(response);
                        break;
                    }
                    None => {
                        log::debug!("Unsupported request. Ignoring...");
                    }
                }
            }
        });

        tokio::select! {
            _ = rx_sleep => {
                self.server.unblock();
                Err::<TResponse, Box<dyn Error>>(Box::new(Timeout {}))
            }
            Ok(response) = rx_server => {
                Ok::<TResponse, Box<dyn Error>>(response)
            }
        }
    }

    pub async fn get_code(
        &self,
        timeout: u64,
        csrf_token: CsrfToken,
    ) -> Result<String, Box<dyn Error>> {
        self.process_request(timeout, move |request| {
            let url = Url::parse(format!("http://localhost{}", request.url()).as_str()).unwrap();
            let state = url.query_pairs().find(|qp| qp.0.eq("state"));
            let code = url.query_pairs().find(|qp| qp.0.eq("code"));

            match (state, code) {
                (Some((_, state)), Some((_, code))) => {
                    if state == *csrf_token.secret() {
                        let code = code.to_string();
                        log::debug!("Given code {}", code);

                        Self::response_with_default_message(request).unwrap();

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

    pub async fn get_token_data(
        &self,
        timeout: u64,
        csrf_token: CsrfToken,
    ) -> Result<TokenInfo, Box<dyn Error>> {
        self.process_request(timeout, move |mut request| {
            let mut body = String::new();
            match request.method() {
                Method::Post => {
                    request.as_reader().read_to_string(&mut body).unwrap();

                    let form_params =
                        form_urlencoded::parse(body.as_bytes())
                            .collect::<Vec<(Cow<str>, Cow<str>)>>();

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
                        Self::response_with_default_message(request).unwrap();

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
}
