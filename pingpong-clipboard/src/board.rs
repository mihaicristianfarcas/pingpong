//! This end's clipboard: what is on it, whether it changed, and putting a
//! copy from the other end on it. `arboard` reads and writes; the change
//! count and the password managers' "concealed" mark are asked of the
//! system directly where it has them.

use std::borrow::Cow;
use std::io::Cursor;
use std::path::Path;

use pingpong_proto::clip::{self, Item};

/// Files beyond this are not shared (the rest of the copy still is).
const MAX_FILES: usize = clip::MAX_TRANSFER - (1 << 20);

pub struct Board {
    cb: Option<arboard::Clipboard>,
}

impl Board {
    pub fn open() -> Board {
        let cb = arboard::Clipboard::new()
            .map_err(|e| tracing::warn!(error = %e, "no clipboard here"))
            .ok();
        Board { cb }
    }

    /// Changes whenever the clipboard does: the system's change count, or
    /// (where there is none) a digest of what is on it.
    pub fn stamp(&mut self) -> Option<u64> {
        #[cfg(target_os = "macos")]
        {
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            Some(pb.changeCount() as u64)
        }
        #[cfg(windows)]
        {
            Some(
                unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
                    as u64,
            )
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            use std::hash::{Hash, Hasher};
            let cb = self.cb.as_mut()?;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            let text = cb.get_text().ok();
            let files = cb.get().file_list().ok();
            text.hash(&mut h);
            files.hash(&mut h);
            if text.is_none() && files.is_none() {
                cb.get_image()
                    .ok()
                    .map(|i| i.bytes.into_owned())
                    .hash(&mut h);
            }
            Some(h.finish())
        }
    }

    /// A password manager put it there, and asked that it not be shared.
    pub fn concealed(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
            let Some(types) = pb.types() else {
                return false;
            };
            types
                .iter()
                .any(|t| t.to_string() == "org.nspasteboard.ConcealedType")
        }
        #[cfg(windows)]
        {
            use windows::core::w;
            use windows::Win32::System::DataExchange::{
                IsClipboardFormatAvailable, RegisterClipboardFormatW,
            };
            unsafe {
                let f =
                    RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
                f != 0 && IsClipboardFormatAvailable(f).is_ok()
            }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            false
        }
    }

    /// What is on the clipboard: files (when `files`), else text, else an
    /// image. None when it holds nothing shared.
    pub fn read(&mut self, files: bool) -> Option<Vec<Item>> {
        let cb = self.cb.as_mut()?;
        if files {
            if let Ok(list) = file_list(cb) {
                if !list.is_empty() {
                    match crate::gather(&list, MAX_FILES) {
                        Some(items) if !items.is_empty() => return Some(items),
                        Some(_) => {}
                        None => tracing::info!(
                            files = list.len(),
                            "files too large to share (or unreadable)"
                        ),
                    }
                }
            }
        }
        if let Ok(text) = cb.get_text() {
            if !text.is_empty() {
                return Some(vec![Item::Text(text)]);
            }
        }
        let image = cb.get_image().ok()?;
        let png = encode_png(image.width as u32, image.height as u32, &image.bytes)?;
        Some(vec![Item::Png(png)])
    }

    /// Why `read` found nothing, for the log (formats and errors, never
    /// contents).
    pub fn why_not(&mut self) -> String {
        let Some(cb) = self.cb.as_mut() else {
            return "no clipboard".into();
        };
        let files = file_list(cb).map(|l| format!("{} paths", l.len()));
        let text = cb
            .get_text()
            .map(|t| format!("{} bytes", t.len()))
            .map_err(|e| {
                format!("{e:?}")
                    .split(" - ")
                    .next()
                    .unwrap_or_default()
                    .to_string()
            });
        let image = cb
            .get_image()
            .map(|i| format!("{}x{}", i.width, i.height))
            .map_err(|e| {
                format!("{e:?}")
                    .split(" - ")
                    .next()
                    .unwrap_or_default()
                    .to_string()
            });
        let e = |r: Result<String, String>| r.unwrap_or_else(|e| e);
        format!(
            "files: {}; text: {}; image: {}; formats: {}",
            e(files),
            e(text),
            e(image),
            formats()
        )
    }

    /// Put a copy from the other end on the clipboard; its files land in
    /// `dir` first.
    pub fn write(&mut self, items: Vec<Item>, dir: &Path) -> Result<(), String> {
        let cb = self.cb.as_mut().ok_or("no clipboard here")?;
        if items
            .iter()
            .any(|i| matches!(i, Item::File { .. } | Item::Dir { .. }))
        {
            let top = crate::land(&items, dir)?;
            return cb.set().file_list(&top).map_err(|e| e.to_string());
        }
        for item in items {
            match item {
                Item::Text(t) => return cb.set_text(t).map_err(|e| e.to_string()),
                Item::Png(p) => {
                    let (width, height, rgba) =
                        decode_png(&p).ok_or("an image that could not be read")?;
                    let image = arboard::ImageData {
                        width: width as usize,
                        height: height as usize,
                        bytes: Cow::Owned(rgba),
                    };
                    return cb.set_image(image).map_err(|e| e.to_string());
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The formats on the clipboard, for the log.
#[cfg(windows)]
fn formats() -> String {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EnumClipboardFormats, GetClipboardFormatNameW, OpenClipboard,
    };
    unsafe {
        if OpenClipboard(None).is_err() {
            return "(could not open)".into();
        }
        let mut out = Vec::new();
        let mut f = 0;
        loop {
            f = EnumClipboardFormats(f);
            if f == 0 {
                break;
            }
            let mut name = [0u16; 128];
            let n = GetClipboardFormatNameW(f, &mut name);
            out.push(if n > 0 {
                format!("{f}={}", String::from_utf16_lossy(&name[..n as usize]))
            } else {
                f.to_string()
            });
        }
        let _ = CloseClipboard();
        out.join(",")
    }
}

#[cfg(not(windows))]
fn formats() -> String {
    String::new()
}

/// The files on the clipboard.
#[cfg(not(windows))]
fn file_list(cb: &mut arboard::Clipboard) -> Result<Vec<std::path::PathBuf>, String> {
    cb.get().file_list().map_err(|e| {
        format!("{e:?}")
            .split(" - ")
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

/// The files on the clipboard (`CF_HDROP`), read here rather than by
/// `arboard`, so that a failure says which step failed.
#[cfg(windows)]
fn file_list(_cb: &mut arboard::Clipboard) -> Result<Vec<std::path::PathBuf>, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_HDROP: u32 = 15;
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() {
            return Err("no files".into());
        }
        // Another program may hold it a moment.
        let mut opened = OpenClipboard(None);
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            opened = OpenClipboard(None);
        }
        opened.map_err(|e| format!("OpenClipboard: {e}"))?;
        let result = (|| {
            let h = GetClipboardData(CF_HDROP).map_err(|e| format!("GetClipboardData: {e}"))?;
            let mem = HGLOBAL(h.0);
            let size = GlobalSize(mem);
            let p = GlobalLock(mem) as *const u8;
            if p.is_null() {
                return Err(format!(
                    "GlobalLock: {}",
                    windows::core::Error::from_win32()
                ));
            }
            let bytes = std::slice::from_raw_parts(p, size);
            let list = parse_dropfiles(bytes);
            let _ = GlobalUnlock(mem);
            list.ok_or_else(|| format!("a file list that could not be read ({size} bytes)"))
        })();
        let _ = CloseClipboard();
        result.map(|names| {
            names
                .into_iter()
                .map(|w| std::path::PathBuf::from(std::ffi::OsString::from_wide(&w)))
                .collect()
        })
    }
}

/// A `DROPFILES` block: its header, then NUL-separated names ending in an
/// empty one (UTF-16 when `fWide`).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_dropfiles(b: &[u8]) -> Option<Vec<Vec<u16>>> {
    let offset = u32::from_le_bytes(b.get(0..4)?.try_into().ok()?) as usize;
    let wide = u32::from_le_bytes(b.get(16..20)?.try_into().ok()?) != 0;
    let rest = b.get(offset..)?;
    let units: Vec<u16> = if wide {
        let (pairs, _) = rest.as_chunks::<2>();
        pairs.iter().map(|&c| u16::from_le_bytes(c)).collect()
    } else {
        rest.iter().map(|&c| c as u16).collect()
    };
    let mut names = Vec::new();
    for name in units.split(|&u| u == 0) {
        if name.is_empty() {
            break;
        }
        names.push(name.to_vec());
    }
    Some(names)
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if rgba.len() != width as usize * height as usize * 4 || width == 0 || height == 0 {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(Cursor::new(&mut out), width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().ok()?;
        w.write_image_data(rgba).ok()?;
    }
    Some(out)
}

/// Width, height and RGBA pixels.
fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_lists_are_read() {
        let mut b = vec![
            20u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
        ];
        for name in ["C:\\a.txt", "D:\\b"] {
            b.extend(name.encode_utf16().flat_map(|u| u.to_le_bytes()));
            b.extend([0, 0]);
        }
        b.extend([0, 0]);
        let names = super::parse_dropfiles(&b).unwrap();
        let names: Vec<String> = names
            .iter()
            .map(|n| String::from_utf16(n).unwrap())
            .collect();
        assert_eq!(names, ["C:\\a.txt", "D:\\b"]);
        assert!(super::parse_dropfiles(&[1, 2]).is_none());
    }

    #[test]
    fn images_survive_the_trip() {
        let rgba: Vec<u8> = (0..4 * 3 * 4).map(|i| (i * 17) as u8).collect();
        let png = super::encode_png(4, 3, &rgba).unwrap();
        assert_eq!(super::decode_png(&png), Some((4, 3, rgba)));
        assert!(super::encode_png(4, 4, &[0; 3]).is_none());
    }
}
