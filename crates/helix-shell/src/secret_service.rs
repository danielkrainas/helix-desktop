//! A small client of the freedesktop Secret Service API (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.NetworkManager.SecretAgent.md`,
//! "Agent-owned secrets"): plain sessions, searching, storing and
//! deleting items of the default collection.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

const SERVICE: &str = "org.freedesktop.secrets";
const PATH: &str = "/org/freedesktop/secrets";
const IFACE: &str = "org.freedesktop.Secret.Service";
const ITEM: &str = "org.freedesktop.Secret.Item";
const DEFAULT: &str = "/org/freedesktop/secrets/aliases/default";

/// A secret as the API carries it: session, parameters, value, content type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

pub struct Secrets {
    conn: Connection,
    session: OwnedObjectPath,
}

impl Secrets {
    /// A plain (unencrypted, on the session bus only) session.
    pub async fn open(conn: &Connection) -> zbus::Result<Self> {
        let reply = conn.call_method(Some(SERVICE), PATH, Some(IFACE), "OpenSession", &("plain", Value::from(""))).await?;
        let (_, session): (OwnedValue, OwnedObjectPath) = reply.body().deserialize()?;
        Ok(Self { conn: conn.clone(), session })
    }

    async fn search(&self, attrs: &HashMap<&str, &str>) -> zbus::Result<Vec<OwnedObjectPath>> {
        let reply = self.conn.call_method(Some(SERVICE), PATH, Some(IFACE), "SearchItems", &(attrs,)).await?;
        let (mut unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = reply.body().deserialize()?;
        if !locked.is_empty() {
            // Unlocking may need a prompt; those that unlock at once count.
            if let Ok(reply) = self.conn.call_method(Some(SERVICE), PATH, Some(IFACE), "Unlock", &(locked,)).await {
                if let Ok((now, _prompt)) = reply.body().deserialize::<(Vec<OwnedObjectPath>, OwnedObjectPath)>() {
                    unlocked.extend(now);
                }
            }
        }
        Ok(unlocked)
    }

    /// The items matching `attrs`: each one's attributes and secret.
    pub async fn lookup(&self, attrs: &HashMap<&str, &str>) -> zbus::Result<Vec<(HashMap<String, String>, Vec<u8>)>> {
        let items = self.search(attrs).await?;
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let reply = self.conn.call_method(Some(SERVICE), PATH, Some(IFACE), "GetSecrets", &(&items, &self.session)).await?;
        let secrets: HashMap<OwnedObjectPath, Secret> = reply.body().deserialize()?;
        let mut out = Vec::new();
        for (item, (_, _, value, _)) in secrets {
            let proxy = zbus::Proxy::new(&self.conn, SERVICE, item.as_str(), ITEM).await?;
            let attributes: HashMap<String, String> = proxy.get_property("Attributes").await.unwrap_or_default();
            out.push((attributes, value));
        }
        Ok(out)
    }

    /// Store `secret` under `label` with `attrs` in the default
    /// collection, replacing an item with the same attributes.
    pub async fn store(&self, label: &str, attrs: &HashMap<&str, &str>, secret: &[u8]) -> zbus::Result<()> {
        let mut props: HashMap<&str, Value<'_>> = HashMap::new();
        props.insert("org.freedesktop.Secret.Item.Label", Value::from(label));
        props.insert("org.freedesktop.Secret.Item.Attributes", Value::from(attrs.clone()));
        let secret = (&self.session, Vec::<u8>::new(), secret.to_vec(), "text/plain");
        self.conn.call_method(Some(SERVICE), DEFAULT, Some("org.freedesktop.Secret.Collection"), "CreateItem", &(props, secret, true)).await?;
        Ok(())
    }

    /// Delete the items matching `attrs`.
    pub async fn delete(&self, attrs: &HashMap<&str, &str>) -> zbus::Result<()> {
        for item in self.search(attrs).await? {
            self.conn.call_method(Some(SERVICE), item.as_str(), Some(ITEM), "Delete", &()).await?;
        }
        Ok(())
    }
}
