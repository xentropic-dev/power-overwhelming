// Fontconfig has already indexed these faces. Keep file sources so font bytes
// are opened by the existing fontdb loader only when a renderer needs them.
use crate::{Database, FaceInfo, Language, Source, Stretch, Style, Weight, ID};
use fontconfig_sys::{self as fc, constants::*, statics::LIB};
use std::{
    collections::HashSet,
    convert::TryFrom,
    ffi::{CStr, OsStr},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::{self, NonNull},
};

struct Config(NonNull<fc::FcConfig>);

impl Config {
    fn load() -> Option<Self> {
        if fc::statics::LIB_RESULT.is_err() {
            return None;
        }
        // Fontconfig returns a new config owned by the caller.
        NonNull::new(unsafe { (LIB.FcInitLoadConfigAndFonts)() }).map(Self)
    }

    fn system_fonts(&self) -> Option<&[*mut fc::FcPattern]> {
        // The font set and its patterns are owned by this config.
        let set = unsafe { (LIB.FcConfigGetFonts)(self.0.as_ptr(), fc::FcSetSystem).as_ref()? };
        if set.nfont <= 0 || set.fonts.is_null() {
            return None;
        }
        // Fontconfig keeps the array alive for the lifetime of the config.
        Some(unsafe { std::slice::from_raw_parts(set.fonts, set.nfont as usize) })
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        // This releases the config and its owned patterns.
        unsafe { (LIB.FcConfigDestroy)(self.0.as_ptr()) }
    }
}

struct Pattern<'a>(&'a fc::FcPattern);

impl Pattern<'_> {
    fn text(&self, property: &CStr, index: i32) -> Option<&CStr> {
        let mut value = ptr::null_mut();
        // These getters do not mutate the pattern. The string is owned by it.
        let result = unsafe {
            (LIB.FcPatternGetString)(
                self.0 as *const _ as *mut _,
                property.as_ptr(),
                index,
                &mut value,
            )
        };
        if result == fc::FcResultMatch && !value.is_null() {
            Some(unsafe { CStr::from_ptr(value.cast()) })
        } else {
            None
        }
    }

    fn number(&self, property: &CStr) -> Option<i32> {
        let mut value = 0;
        // The getter reads an integer into caller-owned storage.
        let result = unsafe {
            (LIB.FcPatternGetInteger)(
                self.0 as *const _ as *mut _,
                property.as_ptr(),
                0,
                &mut value,
            )
        };
        (result == fc::FcResultMatch).then_some(value)
    }

    fn face(&self) -> Option<FaceInfo> {
        let path = PathBuf::from(OsStr::from_bytes(self.text(FC_FILE, 0)?.to_bytes()));
        if !matches!(
            path.extension().and_then(OsStr::to_str),
            Some("ttf" | "otf" | "ttc" | "otc" | "TTF" | "OTF" | "TTC" | "OTC")
        ) {
            return None;
        }

        let index = u32::try_from(self.number(FC_INDEX).unwrap_or(0)).ok()?;
        // FreeType encodes a variable font's named instance in the upper bits.
        // fontdb and its renderers use the underlying face index.
        if index > u16::MAX.into() {
            return None;
        }

        let mut families: Vec<_> = (0..)
            .map_while(|index| {
                let name = self.text(FC_FAMILY, index)?.to_string_lossy().into_owned();
                let language = match self.text(FC_FAMILYLANG, index).map(CStr::to_bytes) {
                    Some(b"en" | b"en-us") => Language::English_UnitedStates,
                    _ => Language::Unknown,
                };
                Some((name, language))
            })
            .collect();
        if families.is_empty() {
            return None;
        }
        if let Some(english) = families
            .iter()
            .position(|family| family.1 == Language::English_UnitedStates)
        {
            families.swap(0, english);
        }

        let style = match self.number(FC_SLANT) {
            Some(FC_SLANT_ITALIC) => Style::Italic,
            Some(FC_SLANT_OBLIQUE) => Style::Oblique,
            _ => Style::Normal,
        };
        let stretch = match self.number(FC_WIDTH) {
            Some(FC_WIDTH_ULTRACONDENSED) => Stretch::UltraCondensed,
            Some(FC_WIDTH_EXTRACONDENSED) => Stretch::ExtraCondensed,
            Some(FC_WIDTH_CONDENSED) => Stretch::Condensed,
            Some(FC_WIDTH_SEMICONDENSED) => Stretch::SemiCondensed,
            Some(FC_WIDTH_SEMIEXPANDED) => Stretch::SemiExpanded,
            Some(FC_WIDTH_EXPANDED) => Stretch::Expanded,
            Some(FC_WIDTH_EXTRAEXPANDED) => Stretch::ExtraExpanded,
            Some(FC_WIDTH_ULTRAEXPANDED) => Stretch::UltraExpanded,
            _ => Stretch::Normal,
        };

        Some(FaceInfo {
            id: ID::dummy(),
            source: Source::File(path),
            index,
            families,
            post_script_name: self
                .text(FC_POSTSCRIPT_NAME, 0)?
                .to_string_lossy()
                .into_owned(),
            style,
            weight: self.number(FC_WEIGHT).map(weight).unwrap_or_default(),
            stretch,
            monospaced: matches!(self.number(FC_SPACING), Some(FC_MONO | FC_CHARCELL)),
        })
    }
}

fn weight(value: i32) -> Weight {
    let scale = [
        (FC_WEIGHT_THIN, 100),
        (FC_WEIGHT_EXTRALIGHT, 200),
        (FC_WEIGHT_LIGHT, 300),
        (FC_WEIGHT_BOOK, 380),
        (FC_WEIGHT_REGULAR, 400),
        (FC_WEIGHT_MEDIUM, 500),
        (FC_WEIGHT_DEMIBOLD, 600),
        (FC_WEIGHT_BOLD, 700),
        (FC_WEIGHT_EXTRABOLD, 800),
        (FC_WEIGHT_BLACK, 900),
        (FC_WEIGHT_EXTRABLACK, 1000),
    ];
    let value = value.clamp(FC_WEIGHT_THIN, FC_WEIGHT_EXTRABLACK);
    for pair in scale.windows(2) {
        let [(left, low), (right, high)] = [pair[0], pair[1]];
        if value <= right {
            return Weight((low + (value - left) * (high - low) / (right - left)) as u16);
        }
    }
    Weight::BLACK
}

/// Returns false for an unavailable or empty system font set so the directory loader can take over.
pub(super) fn load_faces(database: &mut Database) -> bool {
    let Some(config) = Config::load() else {
        return false;
    };
    let Some(fonts) = config.system_fonts() else {
        return false;
    };
    let mut seen = HashSet::new();
    for &font in fonts {
        // These pattern pointers belong to the config and stay valid for this loop.
        let Some(pattern) = (unsafe { font.as_ref() }) else {
            continue;
        };
        let Some(face) = Pattern(pattern).face() else {
            continue;
        };
        let Source::File(path) = &face.source else {
            continue;
        };
        if seen.insert((path.clone(), face.index)) {
            database.push_face_info(face);
        }
    }
    !seen.is_empty()
}
