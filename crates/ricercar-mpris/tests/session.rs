use std::sync::Arc;

use ricercar_core::{Controller, Library};

#[test]
fn registration_and_props() {
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        eprintln!("skip: no session bus (run under dbus-run-session)");
        return;
    }
    let ctl = Arc::new(Controller::new(
        Arc::new(Library::in_memory().unwrap()),
        "null",
    ));
    let Ok(_conn) = ricercar_mpris::serve(ctl.clone()) else {
        eprintln!("skip: could not own bus name");
        return;
    };
    let c = zbus::blocking::Connection::session().unwrap();

    let body = c
        .call_method(
            Some("org.mpris.MediaPlayer2.ricercar"),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.mpris.MediaPlayer2.Player", "PlaybackStatus"),
        )
        .unwrap();
    let (v,): (zbus::zvariant::OwnedValue,) = body.body().deserialize().unwrap();
    let status: String = v.try_into().unwrap();
    assert_eq!(status, "Stopped");

    let body = c
        .call_method(
            Some("org.mpris.MediaPlayer2.ricercar"),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.mpris.MediaPlayer2", "Identity"),
        )
        .unwrap();
    let (v,): (zbus::zvariant::OwnedValue,) = body.body().deserialize().unwrap();
    let id: String = v.try_into().unwrap();
    assert_eq!(id, "ricercar");

    c.call_method(
        Some("org.mpris.MediaPlayer2.ricercar"),
        "/org/mpris/MediaPlayer2",
        Some("org.mpris.MediaPlayer2.Player"),
        "Play",
        &(),
    )
    .unwrap();

    let fixture = format!(
        "file://{}/../ricercar-audio/tests/fixtures/tone_16_441.flac",
        env!("CARGO_MANIFEST_DIR")
    );
    c.call_method(
        Some("org.mpris.MediaPlayer2.ricercar"),
        "/org/mpris/MediaPlayer2",
        Some("org.mpris.MediaPlayer2.Player"),
        "OpenUri",
        &(fixture.as_str(),),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let body = c
        .call_method(
            Some("org.mpris.MediaPlayer2.ricercar"),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.mpris.MediaPlayer2.Player", "Metadata"),
        )
        .unwrap();
    let (v,): (zbus::zvariant::OwnedValue,) = body.body().deserialize().unwrap();
    let meta: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = v.try_into().unwrap();
    assert!(meta.contains_key("mpris:trackid"));
    assert!(meta.contains_key("xesam:url"));

    // Volume (`ricercar-cli volume` goes through it), then fixed volume.
    let set_volume = |v: f64| {
        c.call_method(
            Some("org.mpris.MediaPlayer2.ricercar"),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &(
                "org.mpris.MediaPlayer2.Player",
                "Volume",
                zbus::zvariant::Value::from(v),
            ),
        )
    };
    let volume = || -> f64 {
        let body = c
            .call_method(
                Some("org.mpris.MediaPlayer2.ricercar"),
                "/org/mpris/MediaPlayer2",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.mpris.MediaPlayer2.Player", "Volume"),
            )
            .unwrap();
        let (v,): (zbus::zvariant::OwnedValue,) = body.body().deserialize().unwrap();
        v.try_into().unwrap()
    };
    set_volume(0.3).unwrap();
    assert_eq!(volume(), 0.3);
    assert_eq!(ctl.lock().volume, 30);
    ctl.set_fixed_volume(true);
    assert_eq!(volume(), 1.0);
    // Accepted without error, and ignored.
    set_volume(0.2).unwrap();
    assert_eq!(volume(), 1.0);
    assert_eq!(ctl.lock().volume, 100);
}
