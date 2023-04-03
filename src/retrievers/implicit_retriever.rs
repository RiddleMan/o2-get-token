use crate::args::Arguments;
use crate::auth_server::AuthServer;
use crate::open_authorization_url::open_authorization_url;
use crate::token_info::TokenInfo;
use crate::OAuthClient;
use anyhow::Result;
use async_trait::async_trait;

use super::token_retriever::TokenRetriever;

pub struct ImplicitRetriever<'a> {
    args: &'a Arguments,
    oauth_client: &'a OAuthClient<'a>,
}

impl<'a> ImplicitRetriever<'a> {
    pub fn new<'b>(
        args: &'b Arguments,
        oauth_client: &'b OAuthClient<'b>,
    ) -> ImplicitRetriever<'b> {
        ImplicitRetriever { args, oauth_client }
    }
}

#[async_trait(?Send)]
impl<'a> TokenRetriever for ImplicitRetriever<'a> {
    async fn retrieve(&self) -> Result<TokenInfo> {
        let (url, csrf) = self.oauth_client.implicit_url();

        open_authorization_url(url.as_str(), &self.args.callback_url)?;

        AuthServer::new(&self.args.callback_url)?
            .get_token_data(self.args.timeout, csrf)
            .await
    }
}
