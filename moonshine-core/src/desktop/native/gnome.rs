//! Mutter's native virtual monitors and temporary display configuration.
use super::super::{Mode, Output, Result, Snapshot, Target};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Props = HashMap<String, OwnedValue>;
type Spec = (String, String, String, String);
type NativeMode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Monitor = (Spec, Vec<NativeMode>, Props);
type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);
type AppliedLogical<'a> = (
	i32,
	i32,
	f64,
	u32,
	bool,
	Vec<(String, String, HashMap<&'a str, Value<'a>>)>,
);
type Current = (u32, Vec<Monitor>, Vec<Logical>, Props);
const DISPLAY: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const CAST: &str = "org.gnome.Mutter.ScreenCast";
const CAST_PATH: &str = "/org/gnome/Mutter/ScreenCast";

struct Virtual {
	session: OwnedObjectPath,
	before: Vec<String>,
}
pub(in crate::desktop) struct Gnome {
	connection: zbus::Connection,
	runtime: tokio::runtime::Runtime,
	virtuals: HashMap<String, Virtual>,
}
fn boolean(props: &Props, key: &str) -> bool {
	props.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}
fn number(props: &Props, key: &str) -> Option<u32> {
	props.get(key).and_then(|v| u32::try_from(v).ok())
}
fn key(spec: &Spec) -> String {
	serde_json::to_string(spec).expect("monitor identity serializes")
}
fn display_mode(mode: &NativeMode) -> Mode {
	Mode {
		width: mode.1.max(0) as u32,
		height: mode.2.max(0) as u32,
		refresh_millihertz: (mode.3 * 1000.0).round() as u32,
	}
}
impl Gnome {
	pub fn connect() -> Result<Self> {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.map_err(|e| e.to_string())?;
		let connection = runtime
			.block_on(async {
				zbus::connection::Builder::session()?
					.method_timeout(Duration::from_secs(5))
					.build()
					.await
			})
			.map_err(|e| e.to_string())?;
		runtime.block_on(async {
			let proxy = zbus::Proxy::new(&connection, DISPLAY, DISPLAY_PATH, DISPLAY)
				.await
				.map_err(|e| e.to_string())?;
			if !proxy
				.get_property::<bool>("ApplyMonitorsConfigAllowed")
				.await
				.map_err(|e| e.to_string())?
			{
				return Err("GNOME does not allow display configuration in this session".into());
			}
			let proxy = zbus::Proxy::new(&connection, CAST, CAST_PATH, CAST)
				.await
				.map_err(|e| e.to_string())?;
			if proxy.get_property::<i32>("Version").await.map_err(|e| e.to_string())? < 3 {
				return Err("GNOME lacks platform virtual monitors; update Mutter".into());
			}
			Ok::<_, String>(())
		})?;
		Ok(Self {
			runtime,
			connection,
			virtuals: HashMap::new(),
		})
	}
	fn current(&self) -> Result<Current> {
		self.runtime
			.block_on(async {
				zbus::Proxy::new(&self.connection, DISPLAY, DISPLAY_PATH, DISPLAY)
					.await?
					.call("GetCurrentState", &())
					.await
			})
			.map_err(|e| e.to_string())
	}
	pub fn snapshot(&mut self) -> Result<Snapshot> {
		let (_, monitors, logicals, props) = self.current()?;
		let mut outputs = Vec::new();
		for (spec, modes, properties) in monitors {
			let mode = modes
				.iter()
				.find(|m| boolean(&m.6, "is-current"))
				.or_else(|| modes.iter().find(|m| boolean(&m.6, "is-preferred")))
				.or_else(|| modes.first());
			let logical = logicals.iter().enumerate().find(|(_, l)| l.5.contains(&spec));
			let (group, x, y, scale, transform, priority) = match logical {
				Some((index, l)) => ((index + 1) as u32, l.0, l.1, l.2, l.3, if l.4 { 1 } else { 2 }),
				None => (0, 0, 0, 1.0, 0, 0),
			};
			outputs.push(Output {
				key: key(&spec),
				name: spec.0,
				enabled: logical.is_some(),
				can_disable: true,
				mode: mode.map(display_mode).unwrap_or_default(),
				x,
				y,
				scale,
				transform,
				priority,
				group,
				hdr: number(&properties, "color-mode").unwrap_or(0),
				wcg: 0,
				replication: String::new(),
			});
		}
		Ok(Snapshot {
			outputs,
			layout_mode: number(&props, "layout-mode"),
		})
	}
	pub fn prepare(&mut self, id: &str, mode: Mode) -> Result<Target> {
		let before = self.snapshot()?.outputs.into_iter().map(|o| o.key).collect();
		let path: OwnedObjectPath = self
			.runtime
			.block_on(async {
				zbus::Proxy::new(&self.connection, CAST, CAST_PATH, CAST)
					.await?
					.call("CreateSession", &(HashMap::<&str, Value<'_>>::new(),))
					.await
			})
			.map_err(|e| e.to_string())?;
		// Retain ownership immediately: RecordVirtual/Start can fail after the
		// compositor has created resources that rollback must still close.
		self.virtuals.insert(
			id.into(),
			Virtual {
				session: path.clone(),
				before,
			},
		);
		let node_id = self
			.runtime
			.block_on(async {
				let session = zbus::Proxy::new(
					&self.connection,
					CAST,
					path.clone(),
					"org.gnome.Mutter.ScreenCast.Session",
				)
				.await?;
				let modes = vec![HashMap::from([
					("size", Value::new((mode.width, mode.height))),
					("refresh-rate", Value::from(f64::from(mode.refresh_millihertz) / 1000.0)),
					("is-preferred", Value::from(true)),
				])];
				let properties = HashMap::from([
					("cursor-mode", Value::from(1u32)),
					("is-platform", Value::from(true)),
					("modes", Value::from(modes)),
				]);
				let stream_path: OwnedObjectPath = session.call("RecordVirtual", &(properties,)).await?;
				let stream = zbus::Proxy::new(
					&self.connection,
					CAST,
					stream_path,
					"org.gnome.Mutter.ScreenCast.Stream",
				)
				.await?;
				let mut signals = stream.receive_signal("PipeWireStreamAdded").await?;
				session.call::<_, _, ()>("Start", &()).await?;
				let node = tokio::time::timeout(Duration::from_secs(5), signals.next())
					.await
					.map_err(|_| zbus::Error::Failure("Mutter virtual stream timed out".into()))?
					.ok_or_else(|| zbus::Error::Failure("Mutter closed the virtual stream".into()))?;
				let (node_id,): (u32,) = node.body().deserialize()?;
				Ok::<_, zbus::Error>(node_id)
			})
			.map_err(|e| e.to_string())?;
		Ok(Target {
			id: id.into(),
			node_id,
			serial: None,
			output: String::new(),
			mode,
		})
	}
	pub fn resolve(&mut self, target: &Target) -> Result<Target> {
		let before = &self
			.virtuals
			.get(&target.id)
			.ok_or("Mutter virtual session disappeared")?
			.before;
		let (_, monitors, _, _) = self.current()?;
		let candidates: Vec<_> = monitors
			.iter()
			.filter(|(spec, _, _)| !before.contains(&key(spec)))
			.filter(|(spec, _, _)| spec.1 == "MetaVendor")
			.collect();
		if candidates.len() != 1 {
			return Err("Mutter virtual output identity is not unambiguous".into());
		}
		let mut result = target.clone();
		result.output = key(&candidates[0].0);
		Ok(result)
	}
	pub fn apply(&mut self, desired: &Snapshot) -> Result<()> {
		let (serial, monitors, _, _) = self.current()?;
		let mut groups: std::collections::BTreeMap<u32, AppliedLogical<'_>> = Default::default();
		for output in desired.outputs.iter().filter(|o| o.enabled) {
			let (spec, modes, props) = monitors
				.iter()
				.find(|(s, _, _)| key(s) == output.key)
				.ok_or("GNOME monitor disconnected")?;
			let mode = modes
				.iter()
				.find(|m| {
					let mode = display_mode(m);
					mode.width == output.mode.width
						&& mode.height == output.mode.height
						&& mode.refresh_millihertz.abs_diff(output.mode.refresh_millihertz) <= 100
				})
				.ok_or_else(|| format!("Mutter does not expose requested mode for {}", output.name))?;
			let mut properties = HashMap::new();
			if props.contains_key("color-mode") {
				properties.insert("color-mode", Value::from(output.hdr));
			}
			let group = groups.entry(output.group).or_insert_with(|| {
				(
					output.x,
					output.y,
					output.scale,
					output.transform,
					output.priority == 1,
					Vec::new(),
				)
			});
			group.5.push((spec.0.clone(), mode.0.clone(), properties));
		}
		if groups.is_empty() {
			return Err("Refusing to disable every GNOME output".into());
		}
		let mut properties = HashMap::new();
		if let Some(layout) = desired.layout_mode {
			properties.insert("layout-mode", Value::from(layout));
		}
		let logicals: Vec<_> = groups.into_values().collect();
		self.runtime
			.block_on(async {
				let proxy = zbus::Proxy::new(&self.connection, DISPLAY, DISPLAY_PATH, DISPLAY).await?;
				// Temporary changes avoid overwriting the desktop's saved monitor configuration.
				proxy
					.call::<_, _, ()>("ApplyMonitorsConfig", &(serial, 1u32, logicals, properties))
					.await
			})
			.map_err(|e| e.to_string())
	}
	pub fn retire(&mut self, id: &str) -> Result<()> {
		if let Some(output) = self.virtuals.get(id) {
			self.runtime
				.block_on(async {
					zbus::Proxy::new(
						&self.connection,
						CAST,
						output.session.clone(),
						"org.gnome.Mutter.ScreenCast.Session",
					)
					.await?
					.call::<_, _, ()>("Stop", &())
					.await
				})
				.map_err(|e| e.to_string())?;
			self.virtuals.remove(id);
		}
		Ok(())
	}
}
