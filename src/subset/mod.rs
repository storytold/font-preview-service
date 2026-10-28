//! Preview subsets: one face, a set of codepoints and an instance in, a
//! renamed WOFF2 out. Synchronous and free of I/O.

mod cff;
pub mod closure;
pub mod codepoints;
mod hb;
mod instance;
mod rename;
#[cfg(test)]
mod tests;
mod woff2;

use std::collections::BTreeSet;

use skrifa::FontRef;

pub use cff::CffError;
pub use closure::closure;
pub use instance::Instance;
pub use rename::{Licence, Naming};
pub use woff2::Quality;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read font: {0}")]
    Read(#[from] skrifa::raw::ReadError),
    #[error("HarfBuzz could not subset the font")]
    Subset,
    #[error("cannot write font: {0}")]
    Build(#[from] write_fonts::BuilderError),
    #[error(transparent)]
    Cff(#[from] CffError),
    #[error("generated name {0:?} contains a Reserved Font Name")]
    ReservedName(String),
    #[error("CFF string {0:?} still names the upstream font")]
    CffOldName(String),
    #[error("WOFF2 encoding failed: {0}")]
    Woff2(#[from] woff2::Woff2Error),
}

/// Subsets `font` to `codepoints` (already through [`closure`]) at
/// `instance`, renames it for its licence and encodes it as WOFF2.
pub fn subset(
    font: &[u8],
    codepoints: &BTreeSet<u32>,
    instance: &Instance,
    naming: &Naming,
    quality: Quality,
) -> Result<Vec<u8>, Error> {
    encode_woff2(
        &subset_renamed(font, codepoints, instance, naming)?,
        quality,
    )
}

/// [`subset`] before WOFF2 encoding: the renamed sfnt, for checks that
/// need to read the subset.
pub fn subset_renamed(
    font: &[u8],
    codepoints: &BTreeSet<u32>,
    instance: &Instance,
    naming: &Naming,
) -> Result<Vec<u8>, Error> {
    let sfnt = subset_sfnt(font, codepoints, instance)?;
    rename::rename(&sfnt, naming)
}

/// Encodes a [`subset_renamed`] result as WOFF2.
pub fn encode_woff2(sfnt: &[u8], quality: Quality) -> Result<Vec<u8>, Error> {
    Ok(woff2::encode(sfnt, quality)?)
}

fn subset_sfnt(
    font: &[u8],
    codepoints: &BTreeSet<u32>,
    instance: &Instance,
) -> Result<Vec<u8>, Error> {
    let pins = instance::pins(&FontRef::new(font)?, instance);
    let face = hb::Face::new(font).ok_or(Error::Subset)?;
    let mut input = hb::Input::new().ok_or(Error::Subset)?;
    input.add_flags(harfbuzz_subset_sys::HB_SUBSET_FLAGS_NO_HINTING);
    {
        let mut unicodes = input.unicodes();
        for &cp in codepoints
            .iter()
            .filter(|&&cp| char::from_u32(cp).is_some())
        {
            unicodes.add(cp);
        }
    }
    if keeps_legacy_kern(font) {
        // HarfBuzz has no subsetter for `kern` and drops it by default. Keep
        // the table whole; it indexes by glyph ID, so the GIDs must stay.
        let kern = u32::from_be_bytes(*b"kern");
        input.drop_tables().remove(kern);
        input.no_subset_tables().add(kern);
        input.add_flags(harfbuzz_subset_sys::HB_SUBSET_FLAGS_RETAIN_GIDS);
    }
    {
        let mut name_ids = input.name_ids();
        for id in rename::KEPT_IDS {
            name_ids.add(id);
        }
        if pins.is_none() {
            // fvar and STAT name their axes and instances with IDs from 256.
            name_ids.add_range(256, 32767);
        }
    }
    if let Some(pins) = pins {
        let pinned = input.pin_all_axes_to_default(&face)
            && pins.iter().all(|(tag, value)| {
                input.pin_axis_location(&face, u32::from_be_bytes(tag.to_be_bytes()), *value)
            });
        if !pinned {
            return Err(Error::Subset);
        }
    }
    face.subset(&input).ok_or(Error::Subset)
}

/// True when the font has a `kern` table and no `GPOS` `kern` feature, so HarfBuzz shapes with
/// `kern`. A GPOS that doesn't parse counts as
/// having one, so `kern` is dropped as for any other font.
fn keeps_legacy_kern(font: &[u8]) -> bool {
    use skrifa::raw::TableProvider;
    FontRef::new(font).is_ok_and(|f| {
        f.table_data(skrifa::Tag::new(b"kern")).is_some()
            && (f.table_data(skrifa::Tag::new(b"GPOS")).is_none()
                || f.gpos().is_ok_and(|gpos| {
                    gpos.feature_list().is_ok_and(|list| {
                        list.feature_records()
                            .iter()
                            .all(|r| r.feature_tag() != skrifa::Tag::new(b"kern"))
                    })
                }))
    })
}
