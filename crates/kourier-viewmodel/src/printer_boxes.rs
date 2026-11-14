//! The 3D viewer's printer boxes (`viewer-3d.md`, "By size, names listed"): one row per distinct
//! build volume, smallest first, with the printers that share it. The names are product names
//! and stay untranslated; only the "mm" and the dash around them go through the catalog.
//!
//! The preference `model_box` holds a row's [`PrinterBox::id`] (empty = Off).

use crate::i18n::trfc;

/// One build volume and the printers that have it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrinterBox {
    /// Width (X) in millimetres.
    pub w: u16,
    /// Depth (Y) in millimetres.
    pub d: u16,
    /// Height (Z) in millimetres.
    pub h: u16,
    /// The printers, as the makers write them (not translated).
    pub names: &'static str,
}

/// The rows, smallest first (by width, then depth, then height). Sources: the makers'
/// pages, checked 2026-11-13 (`notes/kourier/viewer-3d.md`, "Build volumes: sources").
pub const PRINTER_BOXES: [PrinterBox; 18] = [
    PrinterBox {
        w: 180,
        d: 180,
        h: 180,
        names: "Bambu Lab A1 mini, Prusa MINI+",
    },
    PrinterBox {
        w: 220,
        d: 220,
        h: 240,
        names: "Creality Ender-3 V3 KE",
    },
    PrinterBox {
        w: 220,
        d: 220,
        h: 250,
        names: "Creality Ender-3 V3, Ender-3 V3 SE, K1, K1C",
    },
    PrinterBox {
        w: 225,
        d: 225,
        h: 265,
        names: "Elegoo Neptune 4 Pro",
    },
    PrinterBox {
        w: 245,
        d: 245,
        h: 240,
        names: "Qidi Q1 Pro",
    },
    PrinterBox {
        w: 250,
        d: 210,
        h: 220,
        names: "Prusa MK4S",
    },
    PrinterBox {
        w: 250,
        d: 220,
        h: 270,
        names: "Prusa CORE One",
    },
    PrinterBox {
        w: 250,
        d: 250,
        h: 250,
        names: "Anycubic Kobra S1",
    },
    PrinterBox {
        w: 250,
        d: 250,
        h: 260,
        names: "Anycubic Kobra 3",
    },
    PrinterBox {
        w: 256,
        d: 256,
        h: 256,
        names: "Bambu Lab A1, P1P, P1S, P2S, X1C, X1E; Elegoo Centauri Carbon",
    },
    PrinterBox {
        w: 256,
        d: 256,
        h: 260,
        names: "Bambu Lab X2D",
    },
    PrinterBox {
        w: 300,
        d: 300,
        h: 300,
        names: "Creality K1 Max",
    },
    PrinterBox {
        w: 300,
        d: 320,
        h: 320,
        names: "Bambu Lab H2D (both nozzles)",
    },
    PrinterBox {
        w: 305,
        d: 305,
        h: 280,
        names: "Qidi Plus4",
    },
    PrinterBox {
        w: 325,
        d: 320,
        h: 325,
        names: "Bambu Lab H2D (one nozzle)",
    },
    PrinterBox {
        w: 340,
        d: 320,
        h: 340,
        names: "Bambu Lab H2S",
    },
    PrinterBox {
        w: 350,
        d: 350,
        h: 350,
        names: "Creality K2 Plus",
    },
    PrinterBox {
        w: 360,
        d: 360,
        h: 360,
        names: "Prusa XL",
    },
];

impl PrinterBox {
    /// The preference value: "256x256x256".
    pub fn id(&self) -> String {
        format!("{}x{}x{}", self.w, self.d, self.h)
    }

    /// The menu label: "256 × 256 × 256 mm — Bambu Lab A1, …".
    pub fn label(&self) -> String {
        trfc(
            "@item:inmenu 3D printer build volume: width, depth, height in millimetres, then the printers that have it",
            "%1 × %2 × %3 mm — %4",
            &[
                &self.w.to_string(),
                &self.d.to_string(),
                &self.h.to_string(),
                self.names,
            ],
        )
    }
}

/// The row with this preference value, if any.
pub fn find(id: &str) -> Option<&'static PrinterBox> {
    PRINTER_BOXES.iter().find(|b| b.id() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_the_notes_eighteen_volumes() {
        let got: Vec<(u16, u16, u16)> = PRINTER_BOXES.iter().map(|b| (b.w, b.d, b.h)).collect();
        assert_eq!(
            got,
            [
                (180, 180, 180),
                (220, 220, 240),
                (220, 220, 250),
                (225, 225, 265),
                (245, 245, 240),
                (250, 210, 220),
                (250, 220, 270),
                (250, 250, 250),
                (250, 250, 260),
                (256, 256, 256),
                (256, 256, 260),
                (300, 300, 300),
                (300, 320, 320),
                (305, 305, 280),
                (325, 320, 325),
                (340, 320, 340),
                (350, 350, 350),
                (360, 360, 360),
            ]
        );
    }

    #[test]
    fn the_rows_are_sorted_by_size_and_distinct() {
        let key = |b: &PrinterBox| (b.w, b.d, b.h);
        for pair in PRINTER_BOXES.windows(2) {
            assert!(key(&pair[0]) < key(&pair[1]), "{pair:?}");
        }
        let a1 = find("256x256x256").expect("A1 row");
        assert!(find("180x180x180").expect("mini").names.contains("A1 mini"));
        assert!(a1.names.contains("Bambu Lab A1,"));
    }

    #[test]
    fn labels_carry_the_size_and_the_untranslated_names() {
        let mini = find("180x180x180").expect("row");
        assert_eq!(
            mini.label(),
            "180 × 180 × 180 mm — Bambu Lab A1 mini, Prusa MINI+"
        );
        assert!(find("").is_none() && find("1x1x1").is_none());
    }
}
