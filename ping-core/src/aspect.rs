//! Stream sizes at the aspect ratios games are made for.
//!
//! A size between the standard ratios makes a game draw a standard-ratio
//! picture inside it and leave strips around it. On a host's virtual
//! display Windows never refreshes those strips, so whatever an overlay
//! draws in them goes stale: at 3024x1900 -- the area below a 14" MacBook
//! Pro's notch in a scaled ("More Space") desktop -- Witcher 3 drew rows
//! 14..1885 and left the Steam FPS counter's top rows and a notification's
//! bottom rows stuck in the strips, as a screenshot taken on the host itself
//! showed. At 3024x1890, 16:10, it filled the screen and the strips were
//! gone. Moonlight's native size on that Mac is 3024x1890 too.
//!
//! So every size Ping asks for by itself -- this display, a scaled desktop,
//! a custom size -- is the largest one at the nearest standard ratio that
//! fits. On that MacBook that leaves 5 rows of black above and below the
//! picture, against 10 rows of strips a game would otherwise leave.

/// Width : height of the displays games support. Ultrawides come in three
/// ratios that are all called 21:9 (2560x1080 and 5120x2160 are 64:27,
/// 3440x1440 is 43:18, 3840x1600 is 12:5); each is listed, or a common
/// panel would lose columns to the one beside it.
const STANDARD_RATIOS: [(u32, u32); 9] = [
    (5, 4),
    (4, 3),
    (3, 2),
    (16, 10),
    (16, 9),
    (64, 27),
    (43, 18),
    (12, 5),
    (32, 9),
];

/// How far, in pixels of the short side, a size may be from a standard
/// ratio and still count as at it: 1366x768 is 16:9 by name, 768.4 rows by
/// arithmetic.
const AT_RATIO_PX: f64 = 2.0;

/// The largest even size at the standard ratio nearest to `width` x
/// `height` that fits inside it. A size already at a standard ratio (within
/// `AT_RATIO_PX`) comes back as it is; a portrait one stays portrait.
pub fn fit_standard_ratio(width: u16, height: u16) -> (u16, u16) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let portrait = height > width;
    let (long, short) = if portrait {
        (height as u32, width as u32)
    } else {
        (width as u32, height as u32)
    };
    let aspect = (long as f64 / short as f64).ln();
    let distance = |&(n, d): &(u32, u32)| (aspect - (n as f64 / d as f64).ln()).abs();
    let (n, d) = *STANDARD_RATIOS
        .iter()
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
        .expect("the list is not empty");
    // Narrower than the ratio: keep the long side, shorten the short one;
    // wider: the other way round.
    let (l, s) = if (short as f64 - (long * d) as f64 / n as f64).abs() <= AT_RATIO_PX {
        (long, short)
    } else if long * d <= short * n {
        (long, long * d / n)
    } else {
        (short * n / d, short)
    };
    let (l, s) = ((l & !1) as u16, (s & !1) as u16);
    if portrait {
        (s, l)
    } else {
        (l, s)
    }
}

#[cfg(test)]
mod tests {
    use super::fit_standard_ratio;

    #[test]
    fn the_area_below_a_notch_in_a_scaled_desktop_becomes_16_by_10() {
        assert_eq!(fit_standard_ratio(3024, 1900), (3024, 1890));
        assert_eq!(fit_standard_ratio(3600, 2262), (3600, 2250));
        assert_eq!(fit_standard_ratio(3456, 2170), (3456, 2160));
    }

    #[test]
    fn sizes_already_at_a_standard_ratio_are_kept() {
        for size in [
            (3024, 1890), // 14" MacBook Pro below the notch
            (3456, 2160), // 16" MacBook Pro below the notch
            (2560, 1600),
            (1920, 1080),
            (3840, 2160),
            (1366, 768),
            (1280, 800), // Steam Deck
            (1280, 1024),
            (1024, 768),
            (2736, 1824), // Surface
            (2560, 1080),
            (3440, 1440),
            (3840, 1600),
            (5120, 2160),
            (5120, 1440),
        ] {
            assert_eq!(fit_standard_ratio(size.0, size.1), size, "{size:?}");
        }
    }

    #[test]
    fn the_fit_never_exceeds_the_display_and_is_even() {
        for (w, h) in [(3024, 1964), (1500, 1001), (2001, 999), (777, 555)] {
            let (fw, fh) = fit_standard_ratio(w, h);
            assert!(fw <= w && fh <= h, "{w}x{h} -> {fw}x{fh}");
            assert!(fw % 2 == 0 && fh % 2 == 0, "{w}x{h} -> {fw}x{fh}");
        }
    }

    #[test]
    fn a_portrait_display_stays_portrait() {
        assert_eq!(fit_standard_ratio(1080, 1920), (1080, 1920));
        assert_eq!(fit_standard_ratio(1890, 3030), (1890, 3024));
    }
}
