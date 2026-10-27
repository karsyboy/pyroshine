//! Lifecycle-only audio routing through the PulseAudio-compatible control API.
//! Samples still flow directly from PipeWire to the existing audio encoder.
use super::Result;
use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::standard::Mainloop;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Route {
	index: u32,
	destination: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct AudioSnapshot {
	pub sink: String,
	default_sink: String,
	routes: BTreeMap<String, Route>,
}
#[derive(Clone)]
struct Sink {
	index: u32,
	name: String,
	module: Option<u32>,
}
#[derive(Clone)]
struct Playback {
	index: u32,
	sink: u32,
	key: String,
}
pub(super) struct Audio {
	context: Context,
	mainloop: Mainloop,
	changed: Rc<Cell<bool>>,
}
impl Audio {
	pub fn connect() -> Result<Self> {
		let mainloop = Mainloop::new().ok_or("Cannot create audio control loop")?;
		let mut context =
			Context::new(&mainloop, "Pyroshine Desktop Guard").ok_or("Cannot create audio control context")?;
		context
			.connect(None, FlagSet::NOAUTOSPAWN, None)
			.map_err(|e| e.to_string().unwrap_or_else(|| "Audio connection failed".into()))?;
		let mut this = Self {
			mainloop,
			context,
			changed: Rc::new(Cell::new(true)),
		};
		let deadline = Instant::now() + Duration::from_secs(3);
		while this.context.get_state() != State::Ready {
			if matches!(this.context.get_state(), State::Failed | State::Terminated) || Instant::now() >= deadline {
				return Err("Cannot reach the user's PulseAudio-compatible server (pipewire-pulse)".into());
			}
			this.pump()?;
			std::thread::sleep(Duration::from_millis(2));
		}
		let changed = this.changed.clone();
		this.context
			.set_subscribe_callback(Some(Box::new(move |_, _, _| changed.set(true))));
		let result = Rc::new(Cell::new(false));
		let slot = result.clone();
		let operation = this.context.subscribe(
			pulse::context::subscribe::InterestMaskSet::SINK_INPUT
				| pulse::context::subscribe::InterestMaskSet::SINK
				| pulse::context::subscribe::InterestMaskSet::SERVER,
			move |ok| slot.set(ok),
		);
		this.wait(operation)?;
		if !result.get() {
			return Err("Audio server refused routing notifications".into());
		}
		Ok(this)
	}
	fn pump(&mut self) -> Result<()> {
		if let pulse::mainloop::standard::IterateResult::Err(error) = self.mainloop.iterate(false) {
			return Err(error.to_string().unwrap_or_else(|| "Audio mainloop failed".into()));
		}
		if matches!(self.context.get_state(), State::Failed | State::Terminated) {
			return Err("Desktop audio server disconnected".into());
		}
		Ok(())
	}
	fn wait<C: ?Sized>(&mut self, mut operation: pulse::operation::Operation<C>) -> Result<()> {
		let deadline = Instant::now() + Duration::from_secs(3);
		while operation.get_state() == pulse::operation::State::Running {
			if Instant::now() >= deadline {
				operation.cancel();
				return Err("Audio control operation timed out".into());
			}
			self.pump()?;
			std::thread::sleep(Duration::from_millis(1));
		}
		if operation.get_state() != pulse::operation::State::Done {
			return Err("Audio control operation was cancelled".into());
		}
		Ok(())
	}
	fn sinks(&mut self) -> Result<Vec<Sink>> {
		let result = Rc::new(RefCell::new(Vec::new()));
		let slot = result.clone();
		let operation = self.context.introspect().get_sink_info_list(move |item| {
			if let ListResult::Item(info) = item {
				slot.borrow_mut().push(Sink {
					index: info.index,
					name: info.name.as_deref().unwrap_or("").into(),
					module: info.owner_module,
				});
			}
		});
		self.wait(operation)?;
		Ok(result.take())
	}
	fn playback(&mut self) -> Result<Vec<Playback>> {
		let result = Rc::new(RefCell::new(Vec::new()));
		let slot = result.clone();
		let operation = self.context.introspect().get_sink_input_info_list(move |item| {
			if let ListResult::Item(info) = item {
				let serial = info.proplist.get_str("object.serial").unwrap_or_default();
				let pid = info.proplist.get_str("application.process.id").unwrap_or_default();
				slot.borrow_mut().push(Playback {
					index: info.index,
					sink: info.sink,
					key: format!("{}:{:?}:{serial}:{pid}", info.index, info.client),
				});
			}
		});
		self.wait(operation)?;
		Ok(result.take())
	}
	fn default_sink(&mut self) -> Result<String> {
		let result = Rc::new(RefCell::new(None));
		let slot = result.clone();
		let operation = self.context.introspect().get_server_info(move |info| {
			*slot.borrow_mut() = info.default_sink_name.as_deref().map(str::to_owned);
		});
		self.wait(operation)?;
		result.take().ok_or_else(|| "Audio server has no default output".into())
	}
	fn set_default(&mut self, name: &str) -> Result<()> {
		let result = Rc::new(Cell::new(false));
		let slot = result.clone();
		let operation = self.context.set_default_sink(name, move |ok| slot.set(ok));
		self.wait(operation)?;
		if !result.get() {
			return Err("Cannot set the desktop audio output".into());
		}
		Ok(())
	}
	fn move_stream(&mut self, index: u32, name: &str) -> Result<()> {
		let result = Rc::new(Cell::new(false));
		let slot = result.clone();
		let operation =
			self.context
				.introspect()
				.move_sink_input_by_name(index, name, Some(Box::new(move |ok| slot.set(ok))));
		self.wait(operation)?;
		// Playback may have ended between enumeration and move; verify below.
		if !result.get() && self.playback()?.iter().any(|p| p.index == index) {
			return Err("Cannot move desktop playback".into());
		}
		Ok(())
	}
	pub fn snapshot(&mut self, lease: &str) -> Result<AudioSnapshot> {
		let sinks = self.sinks()?;
		let mut routes = BTreeMap::new();
		for playback in self.playback()? {
			if let Some(sink) = sinks.iter().find(|s| s.index == playback.sink) {
				routes.insert(
					playback.key,
					Route {
						index: playback.index,
						destination: sink.name.clone(),
					},
				);
			}
		}
		Ok(AudioSnapshot {
			sink: format!("moonshine.desktop.{lease}"),
			default_sink: self.default_sink()?,
			routes,
		})
	}
	pub fn create(&mut self, snapshot: &AudioSnapshot) -> Result<()> {
		if self.sinks()?.iter().any(|s| s.name == snapshot.sink) {
			return Err("Streaming audio sink already exists".into());
		}
		let result = Rc::new(Cell::new(u32::MAX));
		let slot = result.clone();
		let arguments = format!(
			"sink_name={} rate=48000 channels=8 channel_map=front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right sink_properties=device.description=Pyroshine-Desktop",
			snapshot.sink
		);
		let operation = self
			.context
			.introspect()
			.load_module("module-null-sink", &arguments, move |index| slot.set(index));
		self.wait(operation)?;
		if result.get() == u32::MAX || !self.sinks()?.iter().any(|s| s.name == snapshot.sink) {
			return Err("Audio server cannot create a streaming sink".into());
		}
		Ok(())
	}
	/// Called before every routing mutation so new playback destinations can be
	/// journalled before they are changed. Existing records remain immutable.
	pub fn update_snapshot(&mut self, snapshot: &mut AudioSnapshot) -> Result<bool> {
		self.pump()?;
		let sinks = self.sinks()?;
		let playback = self.playback()?;
		let count = snapshot.routes.len();
		snapshot.routes.retain(|key, _| playback.iter().any(|p| p.key == *key));
		let mut changed = count != snapshot.routes.len();
		for playback in playback {
			if !snapshot.routes.contains_key(&playback.key) {
				let destination = sinks
					.iter()
					.find(|s| s.index == playback.sink && s.name != snapshot.sink)
					.map(|s| s.name.clone())
					.unwrap_or_else(|| snapshot.default_sink.clone());
				snapshot.routes.insert(
					playback.key,
					Route {
						index: playback.index,
						destination,
					},
				);
				changed = true;
			}
		}
		Ok(changed)
	}
	pub fn route(&mut self, snapshot: &AudioSnapshot) -> Result<()> {
		let sink = self
			.sinks()?
			.into_iter()
			.find(|s| s.name == snapshot.sink)
			.ok_or("Streaming audio sink disappeared")?;
		if self.default_sink()? != snapshot.sink {
			self.set_default(&snapshot.sink)?;
		}
		for playback in self.playback()? {
			if playback.sink != sink.index {
				self.move_stream(playback.index, &sink.name)?;
			}
		}
		if self.playback()?.iter().any(|p| p.sink != sink.index) {
			return Err("Desktop playback is still routed locally".into());
		}
		self.changed.set(false);
		Ok(())
	}
	pub fn take_changed(&mut self) -> Result<bool> {
		self.pump()?;
		Ok(self.changed.replace(false))
	}
	pub fn changed(&mut self) -> Result<bool> {
		self.pump()?;
		Ok(self.changed.get())
	}
	pub fn restore(&mut self, snapshot: &AudioSnapshot) -> Result<()> {
		let sinks = self.sinks()?;
		let owned = sinks.iter().find(|s| s.name == snapshot.sink);
		let fallback = sinks
			.iter()
			.find(|s| s.name == snapshot.default_sink)
			.or_else(|| sinks.iter().find(|s| s.name != snapshot.sink))
			.ok_or("No desktop audio output is available for recovery")?;
		if self.default_sink()? == snapshot.sink {
			self.set_default(&fallback.name)?;
		}
		if let Some(owned) = owned {
			for playback in self.playback()?.into_iter().filter(|p| p.sink == owned.index) {
				let destination = snapshot
					.routes
					.get(&playback.key)
					.and_then(|r| sinks.iter().find(|s| s.name == r.destination))
					.unwrap_or(fallback);
				self.move_stream(playback.index, &destination.name)?;
			}
			if self.playback()?.iter().any(|p| p.sink == owned.index) {
				return Err("Playback restoration is incomplete".into());
			}
			if let Some(module) = owned.module {
				let result = Rc::new(Cell::new(false));
				let slot = result.clone();
				let operation = self.context.introspect().unload_module(module, move |ok| slot.set(ok));
				self.wait(operation)?;
				if !result.get() {
					return Err("Cannot remove the owned streaming sink".into());
				}
			}
		}
		Ok(())
	}
}
