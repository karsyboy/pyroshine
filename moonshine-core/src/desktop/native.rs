//! Compositor adapters. No session manager, encoder, or transport policy lives here.
mod gnome;
mod kde;

use super::{Mode, Result, Snapshot, Target};
use crate::session::capture::portal::desktop::{DesktopKind, DesktopSession};

pub(super) enum Native {
	Kde(Box<kde::Kde>),
	Gnome(Box<gnome::Gnome>),
	Unavailable(String),
}

impl Native {
	pub fn connect(desktop: &DesktopSession) -> Result<Self> {
		match desktop.kind {
			DesktopKind::Kde => kde::Kde::connect(&desktop.wayland_socket).map(|a| Self::Kde(Box::new(a))),
			DesktopKind::Gnome => gnome::Gnome::connect().map(|a| Self::Gnome(Box::new(a))),
			DesktopKind::Other => Err("Managed Desktop Mode requires KDE Plasma or GNOME on Wayland".into()),
		}
	}
	pub fn snapshot(&mut self) -> Result<Snapshot> {
		match self {
			Self::Kde(a) => a.snapshot(),
			Self::Gnome(a) => a.snapshot(),
			Self::Unavailable(reason) => Err(reason.clone()),
		}
	}
	pub fn prepare(&mut self, id: &str, mode: Mode) -> Result<Target> {
		match self {
			Self::Kde(a) => a.prepare(id, mode),
			Self::Gnome(a) => a.prepare(id, mode),
			Self::Unavailable(reason) => Err(reason.clone()),
		}
	}
	pub fn apply(&mut self, snapshot: &Snapshot) -> Result<()> {
		match self {
			Self::Kde(a) => a.apply(snapshot),
			Self::Gnome(a) => a.apply(snapshot),
			Self::Unavailable(reason) => Err(reason.clone()),
		}
	}
	pub fn retire(&mut self, id: &str) -> Result<()> {
		match self {
			Self::Kde(a) => a.retire(id),
			Self::Gnome(a) => a.retire(id),
			Self::Unavailable(reason) => Err(reason.clone()),
		}
	}
	pub fn pump(&mut self) -> Result<()> {
		match self {
			Self::Kde(a) => a.pump(std::time::Duration::ZERO),
			Self::Gnome(_) | Self::Unavailable(_) => Ok(()),
		}
	}
	pub fn resolve_target(&mut self, target: &Target) -> Result<Target> {
		match self {
			Self::Gnome(a) => a.resolve(target),
			Self::Unavailable(reason) => Err(reason.clone()),
			Self::Kde(_) => Ok(target.clone()),
		}
	}
	pub fn commit(&mut self, target: &Target) -> Result<()> {
		let target = match self {
			Self::Gnome(a) => a.resolve(target)?,
			Self::Kde(_) => target.clone(),
			Self::Unavailable(reason) => return Err(reason.clone()),
		};
		if let Self::Kde(kde) = self {
			kde.retire_other(&target.id)?;
		}
		let mut snapshot = self.snapshot()?;
		let selected = snapshot
			.outputs
			.iter_mut()
			.find(|o| o.key == target.output)
			.ok_or("The prepared virtual output disappeared")?;
		selected.mode = target.mode;
		selected.enabled = true;
		selected.x = 0;
		selected.y = 0;
		selected.scale = 1.0;
		selected.transform = 0;
		selected.priority = 1;
		selected.group = 1;
		selected.hdr = 0;
		selected.wcg = 0;
		selected.replication.clear();
		for output in &mut snapshot.outputs {
			if output.key != target.output {
				if output.enabled && !output.can_disable {
					return Err(format!("Output {} cannot be disabled", output.name));
				}
				output.enabled = false;
			}
		}
		self.apply(&snapshot)?;
		verify(&self.snapshot()?, &snapshot)
	}
}

pub(super) fn verify(observed: &Snapshot, expected: &Snapshot) -> Result<()> {
	if observed.layout_mode != expected.layout_mode {
		return Err("The desktop layout mode was not restored".into());
	}
	// Logical group numbers are compositor-local. Verify mirror membership,
	// rather than comparing identifiers assigned during enumeration.
	for a in expected.outputs.iter().filter(|o| o.enabled) {
		for b in expected.outputs.iter().filter(|o| o.enabled) {
			let observed_a = observed.outputs.iter().find(|o| o.key == a.key);
			let observed_b = observed.outputs.iter().find(|o| o.key == b.key);
			if let (Some(oa), Some(ob)) = (observed_a, observed_b)
				&& (a.group == b.group) != (oa.group == ob.group)
			{
				return Err("The desktop mirror configuration was not restored".into());
			}
		}
	}
	for output in &expected.outputs {
		let found = observed
			.outputs
			.iter()
			.find(|o| o.key == output.key)
			.ok_or_else(|| format!("Output {} is unavailable", output.name))?;
		if !super::matches(found, output) {
			return Err(format!(
				"Output {} did not apply its requested configuration",
				output.name
			));
		}
	}
	if observed
		.outputs
		.iter()
		.any(|o| o.enabled && !expected.outputs.iter().any(|e| e.key == o.key && e.enabled))
	{
		return Err("An unexpected output is still enabled".into());
	}
	Ok(())
}

/// Reconcile stable identities against current hardware. A missing saved output
/// is a partial recovery, never permission to remove the last usable image.
pub(super) fn restore_configuration(
	current: &Snapshot,
	baseline: &Snapshot,
	owned: &[String],
) -> Result<(Snapshot, bool)> {
	let mut partial = false;
	let mut outputs = Vec::new();
	for saved in &baseline.outputs {
		if current.outputs.iter().any(|o| o.key == saved.key) {
			outputs.push(saved.clone());
		} else if saved.enabled {
			partial = true;
		}
	}
	for output in &current.outputs {
		if !outputs.iter().any(|o| o.key == output.key) {
			let mut output = output.clone();
			if owned.contains(&output.key) {
				// KWin virtual outputs lack Disable. Restore physical displays
				// while these stay live, then close their owning screencasts.
				output.enabled = !output.can_disable;
			}
			outputs.push(output);
		}
	}
	if !outputs.iter().any(|o| o.enabled && !owned.contains(&o.key)) {
		let fallback = outputs
			.iter_mut()
			.find(|o| !owned.contains(&o.key) && !o.name.starts_with("Virtual-") && o.mode.width > 0)
			.ok_or("No connected physical output is available for recovery; virtual output retained")?;
		fallback.enabled = true;
		fallback.x = 0;
		fallback.y = 0;
		fallback.priority = 1;
		fallback.group = 1;
		partial = true;
	}
	Ok((
		Snapshot {
			outputs,
			layout_mode: baseline.layout_mode,
		},
		partial,
	))
}

/// Administrative operations are injectable for transaction fault tests. This
/// interface is never called on the frame or input-event paths.
pub(super) trait Platform {
	fn snapshot(&mut self) -> Result<Snapshot>;
	fn prepare(&mut self, id: &str, mode: Mode) -> Result<Target>;
	fn resolve_target(&mut self, target: &Target) -> Result<Target>;
	fn apply(&mut self, snapshot: &Snapshot) -> Result<()>;
	fn commit(&mut self, target: &Target) -> Result<()>;
	fn retire(&mut self, id: &str) -> Result<()>;
	fn pump(&mut self) -> Result<()>;
}
impl Platform for Native {
	fn snapshot(&mut self) -> Result<Snapshot> {
		Native::snapshot(self)
	}
	fn prepare(&mut self, id: &str, mode: Mode) -> Result<Target> {
		Native::prepare(self, id, mode)
	}
	fn resolve_target(&mut self, target: &Target) -> Result<Target> {
		Native::resolve_target(self, target)
	}
	fn apply(&mut self, snapshot: &Snapshot) -> Result<()> {
		Native::apply(self, snapshot)
	}
	fn commit(&mut self, target: &Target) -> Result<()> {
		Native::commit(self, target)
	}
	fn retire(&mut self, id: &str) -> Result<()> {
		Native::retire(self, id)
	}
	fn pump(&mut self) -> Result<()> {
		Native::pump(self)
	}
}
