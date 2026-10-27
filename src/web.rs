//! `pyroshine web`: the web management interface's address and the
//! administrator setup code, from the running server over the same-user
//! management interface (so only the streaming user can obtain a code).

use std::time::Duration;

use moonshine_management::{BUS_NAME, ManagementError, ManagementProxy};

pub enum Action {
	/// Print the address.
	Url,
	/// Print the address and a setup code; `reset` replaces an existing password.
	Setup { reset: bool },
}

async fn connect(bus: Option<String>) -> Result<(zbus::Connection, ManagementProxy<'static>), String> {
	let builder = match &bus {
		Some(address) => zbus::connection::Builder::address(address.as_str()),
		None => zbus::connection::Builder::session(),
	}
	.map_err(|error| bus_error(&error))?;
	let connection = builder
		.method_timeout(Duration::from_secs(10))
		.build()
		.await
		.map_err(|error| bus_error(&error))?;
	let dbus = zbus::fdo::DBusProxy::new(&connection)
		.await
		.map_err(|error| error.to_string())?;
	let name = zbus::names::BusName::try_from(BUS_NAME).expect("valid management bus name");
	if !dbus.name_has_owner(name).await.unwrap_or(false) {
		return Err(
			"Pyroshine is not running for this user. Start it (systemctl start \"pyroshine@$USER\") and run this \
			 command as the streaming user, without sudo."
				.into(),
		);
	}
	let proxy = ManagementProxy::new(&connection)
		.await
		.map_err(|error| error.to_string())?;
	Ok((connection, proxy))
}

fn bus_error(error: &zbus::Error) -> String {
	format!("Cannot reach your session bus: {error}. Run as the streaming user; headless use requires lingering.")
}

fn call_error(error: zbus::Error) -> String {
	let error = ManagementError::from(error);
	match &error {
		ManagementError::ZBus(zbus::Error::MethodError(name, _, _))
			if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
		{
			"The running server predates the web interface. Restart Pyroshine after updating.".into()
		},
		_ => error.message(),
	}
}

pub async fn run(action: Action, bus: Option<String>) -> Result<(), String> {
	let (_connection, proxy) = connect(bus).await?;
	let url = proxy.get_web_url().await.map_err(call_error)?;
	if url.is_empty() {
		return Err(
			"The web management interface is disabled. Set enabled = true in the [management] section of the \
			 configuration and restart Pyroshine."
				.into(),
		);
	}
	match action {
		Action::Url => println!("{url}"),
		Action::Setup { reset } => {
			let code = proxy.create_web_setup_code(reset).await.map_err(call_error)?;
			println!("Open {url} in a browser and enter this setup code within 15 minutes:");
			println!();
			println!("    {code}");
			println!();
			if reset {
				println!("Setting a new password signs out every browser.");
			}
			let port = url
				.trim_end_matches('/')
				.rsplit_once(':')
				.map_or("47990", |(_, port)| port);
			println!("From another computer, forward the port over SSH first and open http://127.0.0.1:{port}/ there:");
			println!("    ssh -L {port}:127.0.0.1:{port} {}@<this-host>", whoami());
		},
	}
	Ok(())
}

fn whoami() -> String {
	std::env::var("USER").unwrap_or_else(|_| "user".into())
}
