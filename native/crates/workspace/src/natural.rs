//! Natural ("human") string ordering: `nb2` < `nb10`, case-insensitive first.
//!
//! Matches what JS `localeCompare(b, undefined, { numeric: true })` gives for
//! typical file names, which is what the Electron app showed.

use std::cmp::Ordering;

/// Compares two file names naturally: digit runs compare by numeric value,
/// everything else compares case-insensitively; ties are broken by fewer
/// leading zeros, then lowercase-before-uppercase, then raw bytes.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    let mut tiebreak = Ordering::Equal;
    while i < ab.len() && j < bb.len() {
        let (ca, cb) = (ab[i], bb[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let si = i;
            while i < ab.len() && ab[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < bb.len() && bb[j].is_ascii_digit() {
                j += 1;
            }
            let (ra, rb) = (&ab[si..i], &bb[sj..j]);
            let za = ra.iter().take_while(|&&c| c == b'0').count();
            let zb = rb.iter().take_while(|&&c| c == b'0').count();
            let (na, nb) = (&ra[za..], &rb[zb..]);
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if ord != Ordering::Equal {
                return ord;
            }
            if tiebreak == Ordering::Equal {
                tiebreak = za.cmp(&zb);
            }
            continue;
        }
        if ca.is_ascii() && cb.is_ascii() {
            let (la, lb) = (ca.to_ascii_lowercase(), cb.to_ascii_lowercase());
            if la != lb {
                return la.cmp(&lb);
            }
            if tiebreak == Ordering::Equal && ca != cb {
                // lowercase first, like ICU collation
                tiebreak = cb.cmp(&ca);
            }
            i += 1;
            j += 1;
            continue;
        }
        // Non-ASCII: compare one char at a time, lowercased.
        let xa = a[i..].chars().next().unwrap_or('\0');
        let xb = b[j..].chars().next().unwrap_or('\0');
        let la = xa.to_lowercase().next().unwrap_or(xa);
        let lb = xb.to_lowercase().next().unwrap_or(xb);
        if la != lb {
            return la.cmp(&lb);
        }
        if tiebreak == Ordering::Equal && xa != xb {
            tiebreak = xb.cmp(&xa);
        }
        i += xa.len_utf8();
        j += xb.len_utf8();
    }
    (ab.len() - i)
        .cmp(&(bb.len() - j))
        .then(tiebreak)
        .then_with(|| ab.cmp(bb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn numeric_runs() {
        assert_eq!(
            sorted(vec!["nb10.ipynb", "nb2.ipynb", "nb1.ipynb", "nb01.ipynb", "nb.ipynb"]),
            vec!["nb.ipynb", "nb1.ipynb", "nb01.ipynb", "nb2.ipynb", "nb10.ipynb"]
        );
        assert_eq!(natural_cmp("v1.10", "v1.9"), Ordering::Greater);
        assert_eq!(natural_cmp("x99999999999999999999999", "x100000000000000000000000"), Ordering::Less);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(sorted(vec!["beta", "Alpha", "alpha", "Gamma"]), vec!["alpha", "Alpha", "beta", "Gamma"]);
        assert_eq!(sorted(vec!["Éclair", "eclair", "zeta", "écrit"]), vec!["eclair", "zeta", "Éclair", "écrit"]);
    }

    #[test]
    fn total_order() {
        assert_eq!(natural_cmp("a", "a"), Ordering::Equal);
        assert_ne!(natural_cmp("a01", "a1"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "ab"), Ordering::Less);
    }
}
