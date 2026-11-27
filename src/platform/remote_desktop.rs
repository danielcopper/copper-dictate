use std::collections::HashMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::future::poll_fn;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures_core::Stream;
use thiserror::Error;
use zbus::wire::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};
use zbus::{Connection, Proxy};

use crate::xdg::{self, Env};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const REQUEST: &str = "org.freedesktop.portal.Request";
const KEYBOARD: u32 = 1;
const PERSIST_UNTIL_REVOKED: u32 = 2;
const TOKEN_FILE: &str = "remote-desktop-token";

#[derive(Debug, Error)]
pub(crate) enum PortalError {
    #[error("the session bus cannot be reached: {0}")]
    Bus(zbus::Error),
    #[error("the desktop portal offers no remote desktop: {0}")]
    Missing(zbus::Error),
    #[error("the remote control permission was denied")]
    Denied,
    #[error("the remote desktop portal ended its {0} request without an answer")]
    Failed(&'static str),
    #[error("the remote desktop portal's answer is not understood: {0}")]
    Answer(zbus::Error),
}

/// A remote desktop session that may emulate a keyboard; it lasts as long as this connection.
pub(crate) struct RemoteDesktop {
    _connection: Connection,
}

pub(crate) fn token_path(env: Env<'_>) -> Option<PathBuf> {
    xdg::state_home(env).map(|state| state.join("copper-dictate").join(TOKEN_FILE))
}

pub(crate) fn read_token(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

pub(crate) fn write_token(path: &Path, token: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(token.as_bytes())
}

/// The object path the portal answers a request on, which a caller must listen at before it
/// sends the request.
pub(crate) fn request_path(unique_name: &str, handle_token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("{PORTAL_PATH}/request/{sender}/{handle_token}")
}

type Started = (RemoteDesktop, OwnedFd, Option<String>);

/// Starts a session with a keyboard and returns its input-emulation socket and the token for
/// the next start; the saved token restores the earlier permission, and the portal asks the
/// user where there is none or it no longer holds.
pub(crate) fn start(restore_token: Option<&str>) -> Result<Started, PortalError> {
    zbus::block_on(start_session(restore_token))
}

async fn start_session(restore_token: Option<&str>) -> Result<Started, PortalError> {
    let connection = Connection::session().await.map_err(PortalError::Bus)?;
    let portal = Proxy::new(&connection, PORTAL, PORTAL_PATH, REMOTE_DESKTOP)
        .await
        .map_err(PortalError::Missing)?;

    let created = request(&connection, &portal, "CreateSession", 0, |mut options| {
        options.insert("session_handle_token", Variant::from("copper_dictate"));
        (options,)
    })
    .await?;
    let session: String = created
        .get("session_handle")
        .and_then(|handle| String::try_from(&**handle).ok())
        .ok_or(PortalError::Failed("CreateSession"))?;
    let session = OwnedObjectPath::try_from(session).map_err(PortalError::Answer)?;

    request(&connection, &portal, "SelectDevices", 1, |mut options| {
        options.insert("types", Variant::from(KEYBOARD));
        options.insert("persist_mode", Variant::from(PERSIST_UNTIL_REVOKED));
        if let Some(token) = restore_token {
            options.insert("restore_token", Variant::from(token.to_owned()));
        }
        (session.as_ref(), options)
    })
    .await?;

    let started = request(&connection, &portal, "Start", 2, |options| {
        (session.as_ref(), "", options)
    })
    .await?;
    let new_token = started
        .get("restore_token")
        .and_then(|token| String::try_from(&**token).ok());

    let options: HashMap<&str, Variant<'_>> = HashMap::new();
    let eis: zbus::wire::OwnedFd = portal
        .call("ConnectToEIS", &(session.as_ref(), options))
        .await
        .map_err(PortalError::Answer)?;
    Ok((
        RemoteDesktop {
            _connection: connection,
        },
        OwnedFd::from(eis),
        new_token,
    ))
}

async fn request<'o, B>(
    connection: &Connection,
    portal: &Proxy<'_>,
    method: &'static str,
    number: u32,
    body: impl FnOnce(HashMap<&'static str, Variant<'o>>) -> B,
) -> Result<HashMap<String, OwnedVariant>, PortalError>
where
    B: serde::Serialize + zbus::wire::DynamicType,
{
    let handle_token = format!("copper_dictate_{number}");
    let unique_name = connection
        .unique_name()
        .map_or_else(String::new, ToString::to_string);
    let path = request_path(&unique_name, &handle_token);
    let request = Proxy::new(
        connection,
        PORTAL,
        ObjectPath::try_from(path.as_str()).map_err(PortalError::Answer)?,
        REQUEST,
    )
    .await
    .map_err(PortalError::Bus)?;
    let mut responses = request
        .receive_signal("Response")
        .await
        .map_err(PortalError::Bus)?;

    let mut options = HashMap::new();
    options.insert("handle_token", Variant::from(handle_token));
    portal
        .call_method(method, &body(options))
        .await
        .map_err(PortalError::Missing)?;

    let response = poll_fn(|context| Pin::new(&mut responses).poll_next(context))
        .await
        .ok_or(PortalError::Failed(method))?;
    let (code, results): (u32, HashMap<String, OwnedVariant>) =
        response.body().deserialize().map_err(PortalError::Answer)?;
    match code {
        0 => Ok(results),
        1 => Err(PortalError::Denied),
        _ => Err(PortalError::Failed(method)),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::{read_token, request_path, token_path, write_token};
    use crate::test_support::TempDir;

    #[test]
    fn the_portal_answers_a_request_under_the_senders_unique_name() {
        assert_eq!(
            request_path(":1.42", "copper_dictate_0"),
            "/org/freedesktop/portal/desktop/request/1_42/copper_dictate_0"
        );
    }

    #[test]
    fn the_token_lies_in_xdg_state_home_or_dot_local_state() {
        let env = |name: &str| match name {
            "XDG_STATE_HOME" => Some(OsString::from("/state")),
            "HOME" => Some(OsString::from("/home/someone")),
            _ => None,
        };
        let home_only = |name: &str| (name == "HOME").then(|| OsString::from("/home/someone"));

        assert_eq!(
            token_path(&env).unwrap(),
            Path::new("/state/copper-dictate/remote-desktop-token")
        );
        assert_eq!(
            token_path(&home_only).unwrap(),
            Path::new("/home/someone/.local/state/copper-dictate/remote-desktop-token")
        );
        assert_eq!(token_path(&|_: &str| None), None);
    }

    #[test]
    fn a_written_token_reads_back_and_only_its_owner_can_read_it() {
        let dir = TempDir::new();
        let path = dir.path().join("state/copper-dictate/remote-desktop-token");

        write_token(&path, "a-longer-first-token").unwrap();
        write_token(&path, "second").unwrap();

        assert_eq!(read_token(&path).as_deref(), Some("second"));
        let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn a_missing_or_empty_token_file_gives_no_token() {
        let dir = TempDir::new();
        let path = dir.path().join("remote-desktop-token");

        assert_eq!(read_token(&path), None);
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(read_token(&path), None);
    }
}
