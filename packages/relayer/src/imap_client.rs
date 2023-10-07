use crate::*;

use anyhow::anyhow;
use async_imap::extensions::idle::{Handle, IdleResponse::*};
use async_imap::Session;
use async_native_tls::TlsStream;
use oauth2::reqwest::async_http_client;
use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use tokio::net::TcpStream;

#[derive(Clone)]
pub(crate) enum ImapAuth {
    Password {
        user_id: String,
        password: String,
    },
    Oauth {
        user_id: String,
        client_id: String,
        client_secret: String,
        auth_url: String,
        token_url: String,
        redirect_url: String,
    },
}

#[derive(Clone)]
pub(crate) struct ImapConfig {
    pub(crate) domain_name: String,
    pub(crate) port: u16,
    pub(crate) auth: ImapAuth,
}

struct OAuth2 {
    user_id: String,
    access_token: String,
}

impl async_imap::Authenticator for OAuth2 {
    type Response = String;

    fn process(&mut self, _: &[u8]) -> Self::Response {
        format!(
            "user={}\x01auth=Bearer {}\x01\x01",
            self.user_id, self.access_token
        )
    }
}

pub(crate) struct ImapClient {
    session: Session<TlsStream<TcpStream>>,
    config: ImapConfig,
}

impl ImapClient {
    pub(crate) async fn new(config: ImapConfig) -> Result<Self> {
        let tcp_stream = TcpStream::connect((&*config.domain_name, config.port)).await?;
        let tls = async_native_tls::TlsConnector::new();
        let tls_stream = tls.connect(&*config.domain_name, tcp_stream).await?;
        let client = async_imap::Client::new(tls_stream);

        let mut session = match config.auth.clone() {
            ImapAuth::Password { user_id, password } => {
                client.login(user_id, password).await.map_err(|e| e.0)
            }
            ImapAuth::Oauth {
                user_id,
                client_id,
                client_secret,
                auth_url,
                token_url,
                redirect_url,
            } => {
                let oauth_client = BasicClient::new(
                    ClientId::new(client_id),
                    Some(ClientSecret::new(client_secret)),
                    AuthUrl::new(auth_url)?,
                    Some(TokenUrl::new(token_url)?),
                )
                .set_redirect_uri(RedirectUrl::new(redirect_url)?);

                let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
                let (auth_url, _) = oauth_client
                    .authorize_url(CsrfToken::new_random)
                    .add_scope(Scope::new("https://mail.google.com/".to_string()))
                    .set_pkce_challenge(pkce_challenge)
                    .url();

                let server = tiny_http::Server::http("127.0.0.1:8000").unwrap();
                webbrowser::open(auth_url.as_ref())?;
                let request = server.recv()?;
                let url = request.url().to_string();
                let auth_code = url.split("code=").collect::<Vec<&str>>()[1]
                    .split('&')
                    .next()
                    .unwrap_or("");
                let response =
                    tiny_http::Response::from_string("You can close this window now.".to_string());
                request.respond(response).unwrap();

                println!("Auth Code that I captured {}", auth_code);

                let token_result = oauth_client
                    .exchange_code(AuthorizationCode::new(auth_code.to_string()))
                    .set_pkce_verifier(pkce_verifier)
                    .request_async(async_http_client)
                    .await?;

                let access_token = serde_json::to_string(token_result.access_token())?;
                let oauthed = OAuth2 {
                    user_id,
                    access_token,
                };

                client
                    .authenticate("XOAUTH2", oauthed)
                    .await
                    .map_err(|e| e.0)
            }
        }?;

        session.select("INBOX").await?;

        println!("ImapClient connected succesfully!");

        Ok(Self { session, config })
    }

    pub(crate) async fn retrieve_new_emails(self) -> Result<Vec<u8>> {
        let ImapClient { session, config } = self;
        let idle = session.idle();
        let imap = Self::wait_new_email(idle).await?;

        // loop {
        //     match self.idle.uid_search("UNSEEN").await {
        //         Ok(uids) => {
        //             let mut fetches = vec![];
        //             for (idx, uid) in uids.into_iter().enumerate() {
        //                 let fetched = self
        //                     .session
        //                     .uid_fetch(uid.to_string(), "(BODY[] ENVELOPE)")
        //                     .await?;
        //                 fetches.push(fetched);
        //             }
        //             return Ok(fetches);
        //         }
        //         Err(e) => {
        //             println!("Connection reset, reconnecting...");
        //             self.reconnect()?;
        //         }
        //     }
        // }

        todo!()
    }

    async fn wait_new_email(
        mut idle: Handle<TlsStream<TcpStream>>,
    ) -> Result<Session<TlsStream<TcpStream>>> {
        let result = idle.wait().0.await?;

        match result {
            NewData(_) => return Ok(idle.done().await?),
            _ => Err(anyhow!("{IMAP_IDLE_ERROR}")),
        }
    }

    async fn reconnect(&mut self) -> Result<()> {
        const MAX_RETRIES: u32 = 5;
        let mut retry_count = 0;

        while retry_count < MAX_RETRIES {
            match ImapClient::new(self.config.clone()).await {
                Ok(new_client) => {
                    self.session = new_client.session;
                    return Ok(());
                }
                Err(_) => {
                    retry_count += 1;
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            }
        }

        Err(anyhow!("{IMAP_RECONNECT_ERROR}"))
    }
}
