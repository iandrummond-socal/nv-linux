//! Dropbox sync support.

pub mod api;
pub mod oauth;

pub use api::{Account, DropboxNotifier, DropboxRemote};

/// Service roots, replaceable for local HTTP tests.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub authorize: String,
    pub api: String,
    pub content: String,
    pub notify: String,
}

impl Endpoints {
    pub fn dropbox() -> Self {
        Self {
            authorize: "https://www.dropbox.com/oauth2/authorize".into(),
            api: "https://api.dropboxapi.com".into(),
            content: "https://content.dropboxapi.com".into(),
            notify: "https://notify.dropboxapi.com".into(),
        }
    }
}

pub(crate) type Response = ureq::http::Response<ureq::Body>;

pub(crate) fn agent(timeout: std::time::Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

// Never format transport errors: they can contain request information.
pub(crate) fn transport(error: ureq::Error) -> crate::remote::RemoteError {
    match error {
        ureq::Error::Http(_) => crate::remote::RemoteError::Other("Invalid HTTP request".into()),
        _ => crate::remote::RemoteError::Offline,
    }
}
