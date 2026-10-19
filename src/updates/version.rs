use std::cmp::Ordering;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}
impl Version {
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > 96 {
            return None;
        }
        let raw = raw.strip_prefix('v').unwrap_or(raw);
        let (raw, build) = raw
            .split_once('+')
            .map_or((raw, None), |(a, b)| (a, Some(b)));
        let identifiers = |part: &str| {
            !part.is_empty()
                && part.split('.').all(|p| {
                    !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                })
        };
        if build.is_some_and(|s| !identifiers(s)) {
            return None;
        }
        let (core, pre) = raw.split_once('-').map_or((raw, ""), |(a, b)| (a, b));
        if raw.contains('-') && !identifiers(pre) {
            return None;
        }
        let parts: Vec<_> = core.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let mut values = [0; 3];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty()
                || !p.bytes().all(|v| v.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
            {
                return None;
            }
            values[i] = p.parse().ok()?;
        }
        let pre: Vec<_> = if pre.is_empty() {
            vec![]
        } else {
            pre.split('.').map(str::to_owned).collect()
        };
        if pre
            .iter()
            .any(|p| p.bytes().all(|v| v.is_ascii_digit()) && p.len() > 1 && p.starts_with('0'))
        {
            return None;
        }
        Some(Self { core: values, pre })
    }
    pub fn prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
    pub fn is_patch_of(&self, current: &Self) -> bool {
        if self <= current {
            return false;
        }
        if self.pre.is_empty() && current.pre.is_empty() {
            return self.core[..2] == current.core[..2];
        }
        self.core == current.core
            && self.pre.len() == 3
            && matches!(current.pre.len(), 2 | 3)
            && matches!(self.pre[0].as_str(), "alpha" | "beta" | "rc")
            && self.pre[..2] == current.pre[..2]
            && self.pre[1].bytes().all(|c| c.is_ascii_digit())
            && self.pre[2].bytes().all(|c| c.is_ascii_digit())
            && (current.pre.len() == 2 || current.pre[2].bytes().all(|c| c.is_ascii_digit()))
    }
}
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let core = self.core.cmp(&other.core);
        if core != Ordering::Equal {
            return core;
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        for (a, b) in self.pre.iter().zip(&other.pre) {
            let an = a.bytes().all(|v| v.is_ascii_digit());
            let bn = b.bytes().all(|v| v.is_ascii_digit());
            let value = match (an, bn) {
                (true, true) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => a.cmp(b),
            };
            if value != Ordering::Equal {
                return value;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semantic_order_and_untrusted_version_paths() {
        let order = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
        ];
        for pair in order.windows(2) {
            assert!(Version::parse(pair[0]).unwrap() < Version::parse(pair[1]).unwrap());
        }
        assert_eq!(Version::parse("v1.0.0+build.1"), Version::parse("1.0.0"));
        for bad in [
            "../1.0.0",
            "1.0",
            "01.0.0",
            "1.0.0-alpha.01",
            "1.0.0-",
            "1.0.0+",
            "1.0.0/evil",
            "1.0.0+..",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }
    #[test]
    fn automatic_patches_stay_in_the_current_release_line() {
        for (current, target) in [
            ("0.1.0-alpha.6", "0.1.0-alpha.6.1"),
            ("0.1.0-alpha.6.1", "0.1.0-alpha.6.12"),
            ("1.0.0-beta.2", "1.0.0-beta.2.1"),
            ("1.0.0-rc.1.1", "1.0.0-rc.1.2"),
            ("0.1.0", "0.1.1"),
            ("1.2.3", "1.2.9"),
        ] {
            assert!(
                Version::parse(target)
                    .unwrap()
                    .is_patch_of(&Version::parse(current).unwrap())
            );
        }
        for (current, target) in [
            ("0.1.0-alpha.6", "0.1.0-alpha.7"),
            ("0.1.0-alpha.6.1", "0.1.0-alpha.7.1"),
            ("0.1.0-alpha.6.2", "0.1.0-alpha.6.1"),
            ("0.1.0-alpha.6.1", "0.1.0-alpha.6.1"),
            ("0.1.0-alpha.6", "0.1.0-beta.6.1"),
            ("0.1.0-alpha.6", "0.1.0"),
            ("0.1.0-alpha.6", "0.1.1-alpha.6.1"),
            ("0.1.0-alpha.6", "0.1.0-alpha.6.fix"),
            ("0.1.0-alpha.6", "0.1.0-alpha.6.1.1"),
            ("1.2.3", "1.3.0"),
            ("1.2.3", "2.2.4"),
            ("1.2.3", "1.2.4-rc.1"),
        ] {
            assert!(
                !Version::parse(target)
                    .unwrap()
                    .is_patch_of(&Version::parse(current).unwrap())
            );
        }
    }
}
