//! The pixels the desktop integration hands over (§8.7, §8.8): notification avatars
//! (`image-data`) and the tray icons, rendered by Qt. Offscreen, `harness = false`.
//!
//! `KOURIER_DUMP_PIXELS=<dir>` also writes them as PAM images (RGB_ALPHA) for a look.

use std::path::{Path, PathBuf};

use cxx_qt_lib::QGuiApplication;
use kourier_app::desktop::{ImageData, Presenter};
use kourier_app::facade::{ConversationDetail, ConversationRow};
use kourier_model::{AccountId, ConvKind, ConversationFeatures, ConversationId};
use kourier_qt::desktop::{AVATAR_SIZE, QtPresenter, TRAY_SIZES};

fn detail(title: &str, is_self: bool) -> ConversationDetail {
    ConversationDetail {
        row: ConversationRow {
            id: ConversationId(1),
            account: AccountId(1),
            kind: ConvKind::Direct,
            is_self,
            title: Some(title.to_owned()),
            peer: None,
            avatar: None,
            preview: None,
            last_activity: 0,
            unread: 1,
            mentions: 0,
            marked_unread: false,
            muted_until: None,
            local_muted_until: None,
            archived: false,
            pinned_order: None,
            blocked: false,
            request_pending: false,
            features_hash: 0,
            has_draft: false,
            hidden: false,
        },
        members: Vec::new(),
        features: ConversationFeatures::default(),
        timer: None,
        draft: None,
    }
}

fn pixel(img: &ImageData, x: i32, y: i32) -> [u8; 4] {
    let i = usize::try_from(y * img.rowstride + x * 4).unwrap_or(0);
    [
        img.data[i],
        img.data[i + 1],
        img.data[i + 2],
        img.data[i + 3],
    ]
}

fn white_pixels(img: &ImageData) -> usize {
    img.data
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] == 255 && p[0] > 240 && p[1] > 240 && p[2] > 240)
        .count()
}

fn dump(dir: Option<&Path>, name: &str, width: i32, height: i32, rgba: &[u8]) {
    let Some(dir) = dir else { return };
    let mut out = format!(
        "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
    )
    .into_bytes();
    out.extend_from_slice(rgba);
    let _ = std::fs::write(dir.join(format!("{name}.pam")), out);
}

fn check(cond: bool, what: &str) -> Result<(), String> {
    if cond { Ok(()) } else { Err(what.to_owned()) }
}

fn run(dump_dir: Option<&Path>) -> Result<(), String> {
    let presenter = QtPresenter::new();

    // A named avatar: an opaque colored disc with white initials, transparent corners.
    let named = presenter
        .avatar("Olena Kovalenko", &detail("Olena Kovalenko", false))
        .ok_or("no avatar")?;
    let size = i32::try_from(AVATAR_SIZE).map_err(|e| e.to_string())?;
    check(named.width == size && named.height == size, "avatar size")?;
    check(named.has_alpha && named.channels == 4, "RGBA")?;
    check(pixel(&named, 0, 0)[3] == 0, "transparent corner")?;
    let edge = pixel(&named, size / 2, 3);
    check(edge[3] == 255, "opaque disc")?;
    check(
        !(edge[0] > 240 && edge[1] > 240 && edge[2] > 240),
        "a colored disc, not white",
    )?;
    let initials = white_pixels(&named);
    check(initials > 50, "white initials")?;
    dump(dump_dir, "avatar-named", size, size, &named.data);
    check(
        pixel(&named, 4, 4)[3] == 0,
        "a circle is cut well inside the corner",
    )?;

    // "Round avatars" off: a square with the theme's small corner. The corner pixel is still
    // cut, the one a little way in is filled, and the picture is the same otherwise.
    let squared = QtPresenter::new().with_round(false);
    let square = squared
        .avatar("Olena Kovalenko", &detail("Olena Kovalenko", false))
        .ok_or("no avatar")?;
    check(
        square.width == size && square.height == size,
        "square avatar size",
    )?;
    check(pixel(&square, 0, 0)[3] == 0, "rounded corner")?;
    check(
        pixel(&square, 4, 4)[3] == 255,
        "a square is filled up to its corner",
    )?;
    check(pixel(&square, size / 2, 0)[3] == 255, "opaque edge")?;
    check(
        white_pixels(&square) == white_pixels(&named),
        "the same initials",
    )?;
    dump(dump_dir, "avatar-square", size, size, &square.data);
    let anon_title = "Signal user \u{22EF}1f2e";
    let anon_square = squared
        .avatar(anon_title, &detail(anon_title, false))
        .ok_or("no avatar")?;
    check(
        pixel(&anon_square, 4, 4)[3] == 255,
        "the glyph's tile is square too",
    )?;
    dump(
        dump_dir,
        "avatar-square-anonymous",
        size,
        size,
        &anon_square.data,
    );

    // The same name gives the same pixels; another name another color.
    let again = presenter
        .avatar("Olena Kovalenko", &detail("Olena Kovalenko", false))
        .ok_or("no avatar")?;
    check(again == named, "stable")?;
    let other = presenter
        .avatar("Ari Kestrel", &detail("Ari Kestrel", false))
        .ok_or("no avatar")?;
    dump(dump_dir, "avatar-other", size, size, &other.data);

    // A placeholder name gets the person glyph (more white than two letters).
    let anon = presenter
        .avatar(anon_title, &detail(anon_title, false))
        .ok_or("no avatar")?;
    check(white_pixels(&anon) > initials, "person glyph")?;
    dump(dump_dir, "avatar-anonymous", size, size, &anon.data);

    // Note to Self has no avatar.
    check(
        presenter
            .avatar("Note to Self", &detail("Note to Self", true))
            .is_none(),
        "no avatar for Note to Self",
    )?;

    // Tray icons: every size, ARGB32, the attention variant differs.
    let (icons, attention) = presenter.tray_icons();
    check(icons.len() == TRAY_SIZES.len(), "all tray sizes")?;
    check(attention.len() == TRAY_SIZES.len(), "all attention sizes")?;
    for (icon, alert) in icons.iter().zip(&attention) {
        let n = usize::try_from(icon.width * icon.height * 4).map_err(|e| e.to_string())?;
        check(icon.argb.len() == n, "ARGB32 bytes")?;
        check(
            icon.argb.as_chunks::<4>().0.iter().any(|p| p[0] == 255),
            "an opaque icon",
        )?;
        check(icon.argb != alert.argb, "attention differs")?;
    }
    if let (Some(icon), Some(alert)) = (icons.last(), attention.last()) {
        // ARGB → RGBA for the dump.
        let rgba = |argb: &[u8]| -> Vec<u8> {
            argb.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[1], p[2], p[3], p[0]])
                .collect()
        };
        dump(dump_dir, "tray", icon.width, icon.height, &rgba(&icon.argb));
        dump(
            dump_dir,
            "tray-attention",
            alert.width,
            alert.height,
            &rgba(&alert.argb),
        );
    }
    Ok(())
}

fn main() {
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before Qt starts.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    cxx_qt::init_crate!(kourier_qt);
    let _app = QGuiApplication::new();
    let dump_dir: Option<PathBuf> = std::env::var_os("KOURIER_DUMP_PIXELS").map(PathBuf::from);
    match run(dump_dir.as_deref()) {
        Ok(()) => println!("test desktop pixels ... ok\n\ntest result: ok. 1 passed; 0 failed"),
        Err(e) => {
            println!(
                "test desktop pixels ... FAILED: {e}\n\ntest result: FAILED. 0 passed; 1 failed"
            );
            std::process::exit(1);
        }
    }
}
