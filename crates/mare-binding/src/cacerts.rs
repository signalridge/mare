//! Custom CA certificates for `mare.http` and `mare.http.websocket`, read once in one place.
//!
//! - `MARE_CA_CERTIFICATE`: one PEM bundle; when set it is the only custom source
//!   (the product-specific variable wins over `SSL_CERT_FILE`).
//! - Otherwise `SSL_CERT_FILE` (a PEM bundle) and `SSL_CERT_DIR` (a `:`-separated list of
//!   directories, `;` on Windows) are both used and add up, as OpenSSL and rustls-native-certs do
//!   (`rustls-native-certs-0.8.5/src/lib.rs:118-124`, `:258-266`: the file first, then every dir).
//! - Empty values count as unset. A named file or directory that can't be read, or that holds no
//!   certificate, is an error with a hint. Inside a directory a bad file is skipped with a warning
//!   (`lib.rs:306-364`: an unreadable or non-PEM file is recorded, the others still load; a dangling
//!   link or a subdirectory is passed over silently; OpenSSL's hash-named links resolve to the same
//!   certificates, so identical certificates are kept once, `lib.rs:274-275`).
//! - The certificates are added to the system roots; there are no bundled roots.

use std::path::{Path, PathBuf};

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

pub const MARE_CA_VAR: &str = "MARE_CA_CERTIFICATE";
pub const SSL_CERT_FILE_VAR: &str = "SSL_CERT_FILE";
pub const SSL_CERT_DIR_VAR: &str = "SSL_CERT_DIR";

const FILE_HINT: &str =
    "point it to a PEM file with one or more CERTIFICATE blocks, or unset it to use the system roots";
const DIR_HINT: &str = "point it to a directory of PEM / .crt certificate files, or unset it to use the system roots";

/// What `load` found: the certificates plus a warning for every directory entry it skipped.
#[derive(Debug, Default)]
pub struct Loaded {
    pub certs: Vec<CertificateDer<'static>>,
    pub warnings: Vec<String>,
}

fn process_env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// The custom CA certificates named by the process environment; skipped files are logged.
pub fn load_from_env() -> Result<Vec<CertificateDer<'static>>, String> {
    let loaded = load(&process_env)?;
    for w in &loaded.warnings {
        tracing::warn!("{w}");
    }
    Ok(loaded.certs)
}

/// Whether any custom CA variable is set (the "no CA certificates" hint is moot then).
pub fn custom_ca_configured() -> bool {
    [MARE_CA_VAR, SSL_CERT_FILE_VAR, SSL_CERT_DIR_VAR].iter().any(|k| process_env(k).is_some())
}

/// The suffix `build_client` appends when the client can't be built and no custom CA is set.
pub fn no_roots_hint() -> &'static str {
    " (if this system has no CA certificates, install them — e.g. the ca-certificates package — or point \
     MARE_CA_CERTIFICATE / SSL_CERT_FILE to a PEM bundle, or SSL_CERT_DIR to a directory of PEM / .crt files)"
}

pub fn load(env: &dyn Fn(&str) -> Option<String>) -> Result<Loaded, String> {
    let mut out = Loaded::default();
    if let Some(path) = env(MARE_CA_VAR) {
        out.certs = read_bundle(MARE_CA_VAR, &path)?;
        return Ok(out);
    }
    let (file, dirs) = (env(SSL_CERT_FILE_VAR), env(SSL_CERT_DIR_VAR));
    if let Some(path) = &file {
        out.certs = read_bundle(SSL_CERT_FILE_VAR, path)?;
    }
    if let Some(dirs) = &dirs {
        for dir in std::env::split_paths(dirs).filter(|p| !p.as_os_str().is_empty()) {
            read_dir(&dir, &mut out)?;
        }
        if file.is_none() && out.certs.is_empty() {
            return Err(format!("{SSL_CERT_DIR_VAR}={dirs}: no certificates found ({DIR_HINT})"));
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.certs.retain(|c| seen.insert(c.as_ref().to_vec()));
    Ok(out)
}

fn read_bundle(var: &str, path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let pem = std::fs::read(path).map_err(|e| format!("{var}={path}: {e} ({FILE_HINT})"))?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{var}={path}: {e} ({FILE_HINT})"))?;
    if certs.is_empty() {
        return Err(format!("{var}={path}: no certificates found ({FILE_HINT})"));
    }
    Ok(certs)
}

fn read_dir(dir: &Path, out: &mut Loaded) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{SSL_CERT_DIR_VAR}={}: {e} ({DIR_HINT})", dir.display());
    let mut paths: Vec<PathBuf> =
        std::fs::read_dir(dir).map_err(err)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>().map_err(err)?;
    paths.sort();
    for path in paths {
        // `metadata` follows links: `openssl rehash` makes `<hash>.0` links to the real files
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, // dangling link
            Err(e) => {
                out.warnings.push(format!("{SSL_CERT_DIR_VAR}: skipping {}: {e}", path.display()));
                continue;
            }
        }
        let pem = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                out.warnings.push(format!("{SSL_CERT_DIR_VAR}: skipping {}: {e}", path.display()));
                continue;
            }
        };
        match CertificateDer::pem_slice_iter(&pem).collect::<Result<Vec<_>, _>>() {
            Ok(c) if !c.is_empty() => out.certs.extend(c),
            Ok(_) => {
                out.warnings.push(format!("{SSL_CERT_DIR_VAR}: skipping {}: no certificates found", path.display()))
            }
            Err(e) => out.warnings.push(format!("{SSL_CERT_DIR_VAR}: skipping {}: {e}", path.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated with `openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes
    // -days 36500 -subj "/CN=mare test ca <x>"`; the private keys were discarded.
    const CA_A: &str = "-----BEGIN CERTIFICATE-----
MIIBiTCCAS+gAwIBAgIUdvGQexBOhwY//tyHIu6ZgTW2+t0wCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwObWFyZSB0ZXN0IGNhIGEwIBcNMjYxMTExMTYzOTQ5WhgPMjEy
NjEwMTgxNjM5NDlaMBkxFzAVBgNVBAMMDm1hcmUgdGVzdCBjYSBhMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAE3cUULCSjlK3uuYxG/IbaPJydK59rD674FkghwzJ3
r0lf87D8IEPFAAPAyBj5T+ePds0g5mi5ofvC7EYoUHpf6KNTMFEwHQYDVR0OBBYE
FMZrSKCNYckZ1q6U2ISVIqamfBkRMB8GA1UdIwQYMBaAFMZrSKCNYckZ1q6U2ISV
IqamfBkRMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIhAKO8t59H
QWtI618MpB5Y91p/Rzc1sSQl+956ZQLLKf33AiBj3IeyoMJGFc90S0QvqZjD5WQW
AGdSbQmSOCP+zV+Q6Q==
-----END CERTIFICATE-----
";
    const CA_B: &str = "-----BEGIN CERTIFICATE-----
MIIBiTCCAS+gAwIBAgIUBWE9RJSM2/Xr/nKEMY6BaC0zCPcwCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwObWFyZSB0ZXN0IGNhIGIwIBcNMjYxMTExMTYzOTQ5WhgPMjEy
NjEwMTgxNjM5NDlaMBkxFzAVBgNVBAMMDm1hcmUgdGVzdCBjYSBiMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEkWRm0fV4RPLHh57WuAwtc8c3CFpNewgJwQtws+17
oJRdPUZf9CboHoxExJXM1E2heyJqQD4mgAHSwZlIJ8MrRKNTMFEwHQYDVR0OBBYE
FFIttUdbYdYdtj5Aq3lJgV3Fiu8iMB8GA1UdIwQYMBaAFFIttUdbYdYdtj5Aq3lJ
gV3Fiu8iMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIhAIVKmqeK
rsG+qyqh7AZws0bgrjQcrcGpBN+PEOuRa+FjAiAIA6wUPMDUu0sQV2sLOR8GOMvB
dyamydYX+21i7sgFXw==
-----END CERTIFICATE-----
";
    const CA_C: &str = "-----BEGIN CERTIFICATE-----
MIIBijCCAS+gAwIBAgIUHjY8K0+xd9DmiQiiAn6c47siBe8wCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwObWFyZSB0ZXN0IGNhIGMwIBcNMjYxMTExMTYzOTU1WhgPMjEy
NjEwMTgxNjM5NTVaMBkxFzAVBgNVBAMMDm1hcmUgdGVzdCBjYSBjMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEzfdWqjwW7VS6lPAHvjFX97HYxuhy5Is26T/MmFob
cAI+dIg+fplae2gkBCvJiU/u99DJn74UCS88Sp9fo35snaNTMFEwHQYDVR0OBBYE
FHOoHB6GsoHTI8kfJjKkaAnYH6a6MB8GA1UdIwQYMBaAFHOoHB6GsoHTI8kfJjKk
aAnYH6a6MA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSQAwRgIhALZLxwaY
Vl5OYuFalyOfv0r+DiBcM1FTDB7ht1Mj6LTUAiEAu2RU6eqTIO70a7bEWbQt9uDH
UYIxA0feId1N6ZRCnZo=
-----END CERTIFICATE-----
";

    fn der(pem: &str) -> Vec<u8> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap().as_ref().to_vec()
    }

    fn ders(l: &Loaded) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = l.certs.iter().map(|c| c.as_ref().to_vec()).collect();
        v.sort();
        v
    }

    fn sorted(pems: &[&str]) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = pems.iter().map(|p| der(p)).collect();
        v.sort();
        v
    }

    fn env<'a>(pairs: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, v)| *n == k && !v.trim().is_empty()).map(|(_, v)| v.clone())
    }

    fn write(dir: &Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p.display().to_string()
    }

    #[test]
    fn a_directory_loads_every_certificate_file() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.pem", CA_A);
        write(d.path(), "b.crt", CA_B);
        let dir = d.path().display().to_string();
        let l = load(&env(&[(SSL_CERT_DIR_VAR, dir)])).unwrap();
        assert_eq!(ders(&l), sorted(&[CA_A, CA_B]));
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
    }

    #[test]
    fn a_colon_separated_list_and_duplicate_certificates_are_handled() {
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(d1.path(), "a.pem", CA_A);
        write(d2.path(), "a-copy.pem", CA_A);
        write(d2.path(), "b.pem", CA_B);
        let joined = std::env::join_paths([d1.path(), d2.path()]).unwrap().to_string_lossy().into_owned();
        let l = load(&env(&[(SSL_CERT_DIR_VAR, joined)])).unwrap();
        assert_eq!(ders(&l), sorted(&[CA_A, CA_B]));
    }

    #[cfg(unix)]
    #[test]
    fn hash_named_links_and_dangling_links_are_fine() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.pem", CA_A);
        std::os::unix::fs::symlink(d.path().join("a.pem"), d.path().join("1a2b3c4d.0")).unwrap();
        std::os::unix::fs::symlink(d.path().join("gone.pem"), d.path().join("dead.0")).unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let l = load(&env(&[(SSL_CERT_DIR_VAR, d.path().display().to_string())])).unwrap();
        assert_eq!(ders(&l), sorted(&[CA_A]));
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
    }

    #[test]
    fn a_non_pem_file_is_skipped_with_a_warning_and_the_valid_one_loads() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "README.txt", "not a certificate\n");
        write(d.path(), "broken.pem", "-----BEGIN CERTIFICATE-----\n!!!not base64!!!\n-----END CERTIFICATE-----\n");
        write(d.path(), "good.pem", CA_B);
        let l = load(&env(&[(SSL_CERT_DIR_VAR, d.path().display().to_string())])).unwrap();
        assert_eq!(ders(&l), sorted(&[CA_B]));
        assert_eq!(l.warnings.len(), 2, "{:?}", l.warnings);
        assert!(l.warnings.iter().any(|w| w.contains("README.txt")), "{:?}", l.warnings);
        assert!(l.warnings.iter().any(|w| w.contains("broken.pem")), "{:?}", l.warnings);
    }

    /// Windows: unreadable by an empty DACL (`mare_shared::private::testing::deny_all`).
    #[test]
    fn an_unreadable_file_is_skipped_with_a_warning_and_the_valid_one_loads() {
        let d = tempfile::tempdir().unwrap();
        let locked = write(d.path(), "locked.pem", CA_A);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        #[cfg(windows)]
        mare_shared::private::testing::deny_all(Path::new(&locked));
        write(d.path(), "good.pem", CA_B);
        #[cfg(unix)]
        if std::fs::read(&locked).is_ok() {
            return; // running as root: permissions don't apply
        }
        #[cfg(windows)]
        assert!(std::fs::read(&locked).is_err(), "an empty DACL denies reading, even to the owner");
        let l = load(&env(&[(SSL_CERT_DIR_VAR, d.path().display().to_string())])).unwrap();
        assert_eq!(ders(&l), sorted(&[CA_B]));
        assert!(l.warnings.iter().any(|w| w.contains("locked.pem")), "{:?}", l.warnings);
    }

    #[test]
    fn a_missing_or_certificate_free_directory_is_an_error_with_a_hint() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("nope").display().to_string();
        let e = load(&env(&[(SSL_CERT_DIR_VAR, missing)])).unwrap_err();
        assert!(e.contains("SSL_CERT_DIR=") && e.contains("directory of PEM"), "{e}");
        write(d.path(), "x.txt", "hello");
        let e = load(&env(&[(SSL_CERT_DIR_VAR, d.path().display().to_string())])).unwrap_err();
        assert!(e.contains("no certificates found") && e.contains("SSL_CERT_DIR"), "{e}");
    }

    #[test]
    fn nothing_set_means_nothing_added() {
        assert!(load(&env(&[(SSL_CERT_DIR_VAR, "  ".into())])).unwrap().certs.is_empty());
    }

    #[test]
    fn precedence_mare_over_ssl_and_file_plus_dir_add_up() {
        let d = tempfile::tempdir().unwrap();
        let mare = write(d.path(), "mare.pem", CA_A);
        let file = write(d.path(), "file.pem", CA_B);
        let cdir = d.path().join("certs");
        std::fs::create_dir(&cdir).unwrap();
        write(&cdir, "c.pem", CA_C);
        let cdir = cdir.display().to_string();
        // MARE_CA_CERTIFICATE alone wins over both
        let l = load(&env(&[(MARE_CA_VAR, mare), (SSL_CERT_FILE_VAR, file.clone()), (SSL_CERT_DIR_VAR, cdir.clone())]))
            .unwrap();
        assert_eq!(ders(&l), sorted(&[CA_A]));
        // SSL_CERT_FILE and SSL_CERT_DIR add up
        let l = load(&env(&[(SSL_CERT_FILE_VAR, file), (SSL_CERT_DIR_VAR, cdir.clone())])).unwrap();
        assert_eq!(ders(&l), sorted(&[CA_B, CA_C]));
        // a broken MARE_CA_CERTIFICATE is an error even when the others are fine
        let e = load(&env(&[(MARE_CA_VAR, "/no/such.pem".into()), (SSL_CERT_DIR_VAR, cdir)])).unwrap_err();
        assert!(e.starts_with("MARE_CA_CERTIFICATE=/no/such.pem"), "{e}");
    }

    #[test]
    fn the_no_roots_hint_mentions_ssl_cert_dir() {
        let h = no_roots_hint();
        assert!(h.contains("SSL_CERT_DIR") && h.contains("SSL_CERT_FILE") && h.contains("MARE_CA_CERTIFICATE"), "{h}");
    }
}
