//! The desktop's light or dark style from the desktop portal
//! (`org.freedesktop.appearance` `color-scheme`), followed while the tray
//! runs so the menu icons match the menu's text.

use futures_util::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

use super::icons::Style;
use super::item::TrayHandle;

#[zbus::proxy(
	interface = "org.freedesktop.portal.Settings",
	default_service = "org.freedesktop.portal.Desktop",
	default_path = "/org/freedesktop/portal/desktop"
)]
trait Settings {
	fn read_one(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;

	#[zbus(signal)]
	fn setting_changed(&self, namespace: &str, key: &str, value: Value<'_>) -> zbus::Result<()>;
}

const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

/// `color-scheme`: 1 prefers dark; 0 (no preference) and 2 are light.
fn style(value: &Value<'_>) -> Style {
	match value {
		Value::Value(inner) => style(inner),
		Value::U32(1) => Style::Dark,
		_ => Style::Light,
	}
}

/// Keep the tray's style current. Without a portal the light default stays.
pub async fn follow(connection: zbus::Connection, tray: TrayHandle) {
	let Ok(settings) = SettingsProxy::new(&connection).await else {
		return;
	};
	// Subscribe first so a change between reading and subscribing is not lost.
	let changes = settings.receive_setting_changed().await;
	match settings.read_one(NAMESPACE, KEY).await {
		Ok(value) => tray.style(style(&value)).await,
		Err(error) => tracing::debug!("The desktop portal reports no color scheme ({error})"),
	}
	let Ok(mut changes) = changes else {
		return;
	};
	while let Some(change) = changes.next().await {
		let Ok(change) = change.args() else {
			continue;
		};
		if change.namespace == NAMESPACE && change.key == KEY {
			tray.style(style(&change.value)).await;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_a_dark_preference_is_dark() {
		assert_eq!(style(&Value::U32(1)), Style::Dark);
		assert_eq!(style(&Value::U32(0)), Style::Light);
		assert_eq!(style(&Value::U32(2)), Style::Light);
		assert_eq!(style(&Value::Value(Box::new(Value::U32(1)))), Style::Dark);
		assert_eq!(style(&Value::from("dark")), Style::Light);
	}
}
