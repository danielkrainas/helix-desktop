//! `org.freedesktop.impl.portal.Email` (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "Email"): opens a new
//! message in the user's mail app, filled in with what the app asked for.

use std::collections::HashMap;

use gio::prelude::*;
use zbus::zvariant::{ObjectPath, OwnedValue};
use zbus::{fdo, interface, ObjectServer};

use crate::screenshot::{response, with_request, Results};

/// What to put in the message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    /// Local paths.
    pub attachments: Vec<String>,
}

fn strings(options: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    options
        .get(key)
        .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect()
}

fn string(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options.get(key).and_then(|v| String::try_from(v.clone()).ok()).filter(|s| !s.is_empty())
}

/// A plausible address: something@something, without characters that
/// would let it add header fields or arguments.
pub fn valid_address(address: &str) -> bool {
    let Some((user, host)) = address.split_once('@') else { return false };
    !user.is_empty()
        && !host.is_empty()
        && !address.starts_with('-')
        && !address.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '&' | ',' | '\'' | '"' | '<' | '>'))
}

/// An attachment as a local path (the frontend passes paths or `file:`
/// URIs).
pub fn attachment_path(item: &str) -> Option<String> {
    if item.starts_with('/') {
        return Some(item.to_owned());
    }
    gio::File::for_uri(item).path().map(|p| p.to_string_lossy().into_owned())
}

impl Message {
    /// The message the options describe; addresses that are not valid
    /// are left out.
    pub fn from_options(options: &HashMap<String, OwnedValue>) -> Self {
        let addresses = |list: Vec<String>| list.into_iter().filter(|a| valid_address(a)).collect::<Vec<_>>();
        let mut to: Vec<String> = string(options, "address").into_iter().collect();
        to.extend(strings(options, "addresses"));
        Self {
            to: addresses(to),
            cc: addresses(strings(options, "cc")),
            bcc: addresses(strings(options, "bcc")),
            subject: string(options, "subject"),
            body: string(options, "body"),
            attachments: strings(options, "attachments").iter().filter_map(|a| attachment_path(a)).collect(),
        }
    }

    /// A `mailto:` URI (RFC 6068), with each attachment as an `attach`
    /// field (which Evolution, Geary's handler and others read).
    pub fn mailto(&self) -> String {
        let mut uri = format!("mailto:{}", self.to.iter().map(|a| encode(a)).collect::<Vec<_>>().join(","));
        let mut fields: Vec<(&str, String)> = Vec::new();
        if !self.cc.is_empty() {
            fields.push(("cc", self.cc.join(",")));
        }
        if !self.bcc.is_empty() {
            fields.push(("bcc", self.bcc.join(",")));
        }
        if let Some(subject) = &self.subject {
            fields.push(("subject", subject.clone()));
        }
        if let Some(body) = &self.body {
            fields.push(("body", body.clone()));
        }
        for path in &self.attachments {
            fields.push(("attach", path.clone()));
        }
        for (i, (name, value)) in fields.iter().enumerate() {
            uri.push(if i == 0 { '?' } else { '&' });
            uri.push_str(name);
            uri.push('=');
            uri.push_str(&encode(value));
        }
        uri
    }

    /// Thunderbird's `-compose` argument (it ignores attachments in
    /// `mailto:` URIs).
    pub fn thunderbird_compose(&self) -> String {
        // Values are single-quoted; a quote inside ends the value, so it is
        // left out.
        let quote = |s: &str| format!("'{}'", s.replace('\'', ""));
        let mut parts = Vec::new();
        if !self.to.is_empty() {
            parts.push(format!("to={}", quote(&self.to.join(","))));
        }
        if !self.cc.is_empty() {
            parts.push(format!("cc={}", quote(&self.cc.join(","))));
        }
        if !self.bcc.is_empty() {
            parts.push(format!("bcc={}", quote(&self.bcc.join(","))));
        }
        if let Some(subject) = &self.subject {
            parts.push(format!("subject={}", quote(subject)));
        }
        if let Some(body) = &self.body {
            parts.push(format!("body={}", quote(body)));
        }
        if !self.attachments.is_empty() {
            let uris: Vec<String> = self.attachments.iter().map(|p| gio::File::for_path(p).uri().to_string()).collect();
            parts.push(format!("attachment={}", quote(&uris.join(","))));
        }
        parts.join(",")
    }
}

/// Percent-encode everything but RFC 3986's unreserved characters and `@`.
pub fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether `app` is Thunderbird run directly (not through a sandbox
/// launcher, whose arguments differ).
fn is_thunderbird(app: &gio::AppInfo) -> bool {
    let exe = app.executable();
    exe.file_name().is_some_and(|n| n.to_string_lossy().starts_with("thunderbird"))
}

/// Open the message in the default mail app.
fn compose(message: &Message, token: Option<&str>) -> Result<String, String> {
    let app = gio::AppInfo::default_for_uri_scheme("mailto").ok_or("no app handles mailto: links")?;
    let name = app.name().to_string();
    let context = gio::AppLaunchContext::new();
    if let Some(token) = token {
        context.setenv("XDG_ACTIVATION_TOKEN", token);
        context.setenv("DESKTOP_STARTUP_ID", token);
    }
    if is_thunderbird(&app) {
        let mut cmd = std::process::Command::new(app.executable());
        cmd.arg("-compose").arg(message.thunderbird_compose());
        if let Some(token) = token {
            cmd.env("XDG_ACTIVATION_TOKEN", token).env("DESKTOP_STARTUP_ID", token);
        }
        cmd.spawn().map_err(|e| format!("cannot start {name}: {e}"))?;
    } else {
        app.launch_uris(&[&message.mailto()], Some(&context)).map_err(|e| format!("cannot start {name}: {e}"))?;
    }
    Ok(name)
}

pub struct EmailBackend;

#[interface(name = "org.freedesktop.impl.portal.Email")]
impl EmailBackend {
    async fn compose_email(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let message = Message::from_options(&options);
        let token = string(&options, "activation_token");
        tracing::info!(app_id, to = message.to.len(), attachments = message.attachments.len(), "compose email");
        Ok(with_request(server, handle, || async move {
            let result = tokio::task::spawn_blocking(move || compose(&message, token.as_deref())).await;
            match result {
                Ok(Ok(app)) => {
                    tracing::info!(app, "mail app opened");
                    (response::SUCCESS, Results::new())
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "cannot compose the email");
                    (response::OTHER, Results::new())
                }
                Err(_) => (response::OTHER, Results::new()),
            }
        })
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialogs::value;
    use zbus::zvariant::Value;

    fn options(pairs: &[(&str, Value<'_>)]) -> HashMap<String, OwnedValue> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), value(v.clone()))).collect()
    }

    #[test]
    fn mailto_uri() {
        let o = options(&[
            ("address", Value::from("a@example.org")),
            ("addresses", Value::from(vec!["b@example.org", "bad address", "-x@y"])),
            ("cc", Value::from(vec!["c@example.org"])),
            ("subject", Value::from("Hi & bye")),
            ("body", Value::from("line 1\nline 2")),
            ("attachments", Value::from(vec!["file:///tmp/a%20b.txt", "/tmp/c.pdf"])),
        ]);
        let m = Message::from_options(&o);
        assert_eq!(m.to, ["a@example.org", "b@example.org"]);
        assert_eq!(m.attachments, ["/tmp/a b.txt", "/tmp/c.pdf"]);
        assert_eq!(
            m.mailto(),
            "mailto:a@example.org,b@example.org?cc=c@example.org&subject=Hi%20%26%20bye&body=line%201%0Aline%202&attach=%2Ftmp%2Fa%20b.txt&attach=%2Ftmp%2Fc.pdf"
        );
        assert_eq!(Message::default().mailto(), "mailto:");
    }

    #[test]
    fn thunderbird_argument() {
        let m = Message {
            to: vec!["a@example.org".into()],
            subject: Some("it's".into()),
            attachments: vec!["/tmp/x y".into()],
            ..Default::default()
        };
        assert_eq!(m.thunderbird_compose(), "to='a@example.org',subject='its',attachment='file:///tmp/x%20y'");
    }
}
