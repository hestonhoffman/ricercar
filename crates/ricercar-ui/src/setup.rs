//! Audio setup: pick the interface language, an output, see what its DAC
//! accepts, choose fixed volume. Shown once at the first start (`[ui]
//! setup_done`); Settings → Audio output opens it again, at the
//! language too. The language applies (and is saved) at once, like in the
//! settings; nothing else is applied before the last step.
//!
//! The rows come from the settings' device list (`extras::refresh_devices`,
//! which calls `refresh` here), so capabilities read in the background show
//! up as they arrive.

use std::cell::RefCell;
use std::rc::Rc;

use ricercar_core::config::Config;
use slint::{ModelRc, VecModel};

use crate::DeviceRow;
use crate::app::{Ui, with_ui};
use crate::text::t;

thread_local! {
    /// The output picked in the assistant while it is open.
    static CHOICE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Open the assistant at startup? Not once finished or skipped, and not
/// for users who already chose an output (upgrades, `--device`).
pub fn should_show(cfg: &Config) -> bool {
    !cfg.ui.setup_done && matches!(cfg.audio.device.as_str(), "" | "default")
}

/// Listed outputs, and a PipeWire one chosen earlier; not the placeholder
/// rows of the null and file sinks.
fn offerable(r: &DeviceRow) -> bool {
    r.hardware || r.shared || r.absent
}

/// Outputs offered: hardware first, then shared ones, in list order.
pub fn offered(rows: &[DeviceRow]) -> Vec<DeviceRow> {
    let mut out: Vec<DeviceRow> = rows.iter().filter(|r| offerable(r)).cloned().collect();
    out.sort_by_key(|r| !r.hardware);
    for r in &mut out {
        r.selected = false;
        r.expanded = false;
        if r.hardware {
            r.desc = crate::outputs::card_label(&r.desc).into();
        }
    }
    out
}

/// The output selected when the assistant opens: the current one when
/// offered and bit-perfect, else the first USB card (most likely the DAC),
/// else the current one, else the first offered.
pub fn initial_choice(rows: &[DeviceRow], current: &str) -> String {
    let mut rows = rows.iter().filter(|r| offerable(r));
    let current_row = rows.clone().find(|r| r.name == current);
    if let Some(r) = current_row.filter(|r| r.hardware) {
        return r.name.to_string();
    }
    let usb = rows
        .clone()
        .find(|r| r.hardware && r.desc.ends_with(": USB Audio"));
    usb.or(current_row)
        .or(rows.next())
        .map(|r| r.name.to_string())
        .unwrap_or_else(|| current.to_string())
}

pub fn open(ui: &Rc<Ui>) {
    let (fixed, lang) = {
        let cfg = ui.ctx.config.read().unwrap();
        (cfg.audio.fixed_volume, cfg.ui.language.clone())
    };
    CHOICE.with(|c| *c.borrow_mut() = None);
    let app = ui.app();
    app.set_setup_step(0);
    app.set_ui_language(crate::text::language_index(&lang));
    app.set_setup_fixed(fixed);
    app.set_setup_open(true);
    // Lists the outputs (and calls `refresh`), then reads the cards.
    crate::extras::refresh_devices(ui);
    crate::extras::probe_devices(ui);
}

/// Fill the assistant from the device rows of the settings.
pub fn refresh(ui: &Ui, rows: &[DeviceRow]) {
    let app = ui.app();
    if !app.get_setup_open() {
        return;
    }
    let choice = CHOICE.with(|c| {
        c.borrow_mut()
            .get_or_insert_with(|| initial_choice(rows, &ui.ctx.ctl.device_name()))
            .clone()
    });
    let mut offered = offered(rows);
    for r in &mut offered {
        r.selected = r.name == choice.as_str();
    }
    let dev = offered
        .iter()
        .find(|r| r.selected)
        .cloned()
        .unwrap_or_default();
    let (verdict, note) = if dev.hardware {
        let caps = ui.saved_state.borrow().dac_caps.get(&choice).cloned();
        match caps {
            Some(c) => (crate::text::dac_verdict(crate::dac::best(&c)), ""),
            None if crate::extras::probing(ui, &choice) => (String::new(), ""),
            None => (
                String::new(),
                t("Its capabilities could not be read (it may be busy). You can continue anyway."),
            ),
        }
    } else {
        (
            String::new(),
            t(
                "A shared output goes through the system mixer: the sound server may resample. Capabilities are read on hardware outputs only.",
            ),
        )
    };
    app.set_setup_outputs(ModelRc::new(VecModel::from(offered)));
    app.set_setup_dev(dev);
    app.set_setup_verdict(verdict.into());
    app.set_setup_note(note.into());
}

/// A language picked: saved and applied at once, as in the settings, so
/// the assistant goes on in it (and Later keeps it).
fn pick_language(ui: &Rc<Ui>, index: i32) {
    let pref = crate::text::language_pref(index);
    ui.ctx.update_config(|c| c.ui.language = pref.into());
    ui.app().set_ui_language(index);
    crate::extras::apply_language(ui);
}

fn close(ui: &Ui) {
    CHOICE.with(|c| *c.borrow_mut() = None);
    ui.app().set_setup_open(false);
}

/// Later: nothing changes, and the assistant is not shown again.
fn skip(ui: &Ui) {
    if !ui.ctx.config.read().unwrap().ui.setup_done {
        ui.ctx.update_config(|c| c.ui.setup_done = true);
    }
    close(ui);
}

/// Save the choices and play on the chosen output.
fn finish(ui: &Ui) {
    let fixed = ui.app().get_setup_fixed();
    let choice = CHOICE.with(|c| c.borrow().clone());
    ui.ctx.update_config(|c| {
        c.audio.fixed_volume = fixed;
        c.ui.setup_done = true;
    });
    ui.app().set_fixed_volume(fixed);
    if let Some(name) = choice.filter(|n| *n != ui.ctx.ctl.device_name()) {
        crate::extras::select_device(ui, &name);
    }
    close(ui);
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_setup_start(|| with_ui(open));
    app.on_setup_language(|i| with_ui(|ui| pick_language(ui, i)));
    app.on_setup_pick(|name| {
        with_ui(|ui| {
            CHOICE.with(|c| *c.borrow_mut() = Some(name.to_string()));
            crate::extras::refresh_devices(ui);
        })
    });
    app.on_setup_skip(|| with_ui(|ui| skip(ui)));
    app.on_setup_finish(|| with_ui(|ui| finish(ui)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, desc: &str, hardware: bool) -> DeviceRow {
        DeviceRow {
            name: name.into(),
            desc: desc.into(),
            hardware,
            shared: !hardware,
            ..Default::default()
        }
    }

    #[test]
    fn shown_once_to_new_users() {
        let mut c = Config::default();
        assert!(should_show(&c));
        c.ui.setup_done = true;
        assert!(!should_show(&c));
    }

    #[test]
    fn not_shown_after_an_upgrade_with_a_chosen_output() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "[audio]\ndevice = \"hw:2,0\"\n").unwrap();
        assert!(!should_show(&Config::load(&p)));
        // Never chose one: shown.
        std::fs::write(&p, "[audio]\nfixed_volume = true\n").unwrap();
        assert!(should_show(&Config::load(&p)));
        // Skipped once: saved, and not shown again.
        let mut c = Config::load(&p);
        c.ui.setup_done = true;
        c.save(&p).unwrap();
        assert!(!should_show(&Config::load(&p)));
    }

    #[test]
    fn hardware_first() {
        let rows = [
            row("default", "ALSA default", false),
            row("hw:0,0", "HDA Intel PCH: ALC257 Analog", true),
            row("pipewire", "PipeWire", false),
            row("hw:2,0", "BIG KNOB STUDIO: USB Audio", true),
            DeviceRow {
                name: "Discards audio (testing)".into(),
                desc: "Null sink".into(),
                selected: true,
                ..Default::default()
            },
        ];
        let names: Vec<String> = offered(&rows).iter().map(|r| r.name.to_string()).collect();
        assert_eq!(names, ["hw:0,0", "hw:2,0", "default", "pipewire"]);
        assert_eq!(offered(&rows)[1].desc, "BIG KNOB STUDIO");
        assert!(offered(&rows).iter().all(|r| !r.selected));
    }

    #[test]
    fn first_choice() {
        let rows = [
            row("hw:0,0", "HDA Intel PCH: ALC257 Analog", true),
            row("hw:2,0", "BIG KNOB STUDIO: USB Audio", true),
            row("default", "ALSA default", false),
        ];
        // A USB card over the default output.
        assert_eq!(initial_choice(&rows, "default"), "hw:2,0");
        // A hardware output already chosen stays.
        assert_eq!(initial_choice(&rows, "hw:0,0"), "hw:0,0");
        // No USB card: the current output.
        assert_eq!(initial_choice(&rows[..1], "hw:9,0"), "hw:0,0");
        assert_eq!(
            initial_choice(&[rows[0].clone(), rows[2].clone()], "default"),
            "default"
        );
        assert_eq!(initial_choice(&[], "default"), "default");
    }
}
