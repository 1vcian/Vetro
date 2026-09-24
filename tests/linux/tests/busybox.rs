//! BusyBox statica (musl, Alpine): i comandi principali danno lo stesso
//! risultato su Vetro e su QEMU. Criterio di uscita di M2 (parte BusyBox).

use std::path::PathBuf;
use vetro_linux_tests::{Case, case, guest_bin};

/// Applet raggiungibili dal PATH dei test (solo BusyBox: nessun binario
/// dell'host o del container dell'oracolo).
const APPLETS: &[&str] = &[
    "sh",
    "cat",
    "sort",
    "uniq",
    "wc",
    "head",
    "tail",
    "tr",
    "echo",
    "gzip",
    "gunzip",
    "cmp",
    "ls",
    "xargs",
    "seq",
    "expr",
    "grep",
    "env",
    "touch",
    "basename",
    "dirname",
    "realpath",
    "mkdir",
    "cp",
    "mv",
    "rm",
    "ln",
    "tar",
    "printf",
    "sleep",
    "kill",
    "true",
    "false",
    "date",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "awk",
    "sed",
    "cut",
    "od",
];

const FRUTTA: &[u8] = b"banana\nmela\npera\nbanana\nkiwi\n10\n9\n100\n";

fn bb(test: &str) -> Option<PathBuf> {
    guest_bin("busybox", test)
}

/// Caso con un file di dati e una directory `bin/` di link ai comandi.
fn with_env(name: &str, b: &std::path::Path, args: &[&str]) -> Case {
    let mut c = case(name, b, args).file("frutta.txt", FRUTTA).env("LC_ALL", "C").env("PATH", "{wd}/bin");
    for applet in APPLETS {
        c = c.link(&format!("bin/{applet}"), b);
    }
    c
}

macro_rules! bbtest {
    ($name:ident, [$($arg:expr),*] $(, $extra:ident($($x:expr),*))*) => {
        #[test]
        fn $name() {
            let Some(b) = bb(stringify!($name)) else { return };
            with_env(stringify!($name), &b, &[$($arg),*])
                $(.$extra($($x),*))*
                .check();
        }
    };
}

bbtest!(echo, ["echo", "ciao", "mondo"]);
bbtest!(cat, ["cat", "frutta.txt"]);
bbtest!(wc, ["wc", "frutta.txt"]);
bbtest!(sort_plain, ["sort", "frutta.txt"]);
bbtest!(sort_numeric_reverse, ["sort", "-n", "-r", "frutta.txt"]);
bbtest!(sort_unique, ["sort", "-u", "frutta.txt"]);
bbtest!(uniq_count, ["uniq", "-c", "frutta.txt"]);
bbtest!(head_tail, ["sh", "-c", "head -n 3 frutta.txt; tail -n 2 frutta.txt"]);
bbtest!(cut_chars, ["cut", "-c", "1-3", "frutta.txt"]);
bbtest!(tr_stdin, ["tr", "a-z", "A-Z"], stdin(b"stdin minuscolo\n"));
bbtest!(
    grep_variants,
    [
        "sh",
        "-c",
        "grep -n an frutta.txt; grep -c a frutta.txt; grep -v a frutta.txt; grep -E '^[0-9]+$' frutta.txt"
    ]
);
bbtest!(sed_subst, ["sed", "-e", "s/a/A/g", "-e", "2d", "frutta.txt"]);
bbtest!(
    awk_fields,
    ["awk", "{ n += length($1); print NR, $1 } END { print \"totale\", n, n / NR }", "frutta.txt"]
);
bbtest!(
    seq_expr_printf,
    ["sh", "-c", "seq 1 3; seq 0 0.5 2; expr 6 \\* 7; printf '%05d|%-4s|%x|%.3f\\n' 42 ab 255 3.14159"]
);
bbtest!(od_hexdump, ["od", "-A", "x", "-t", "x1z", "frutta.txt"]);
bbtest!(factor, ["factor", "360", "97", "1000000007"]);
bbtest!(
    paths,
    ["sh", "-c", "basename /a/b/c.txt .txt; dirname /a/b/c.txt; realpath -s ./x/../frutta.txt | wc -c"]
);
bbtest!(
    checksums,
    ["sh", "-c", "md5sum frutta.txt; sha1sum frutta.txt; sha256sum frutta.txt; sha512sum frutta.txt"]
);
bbtest!(gzip_roundtrip, ["sh", "-c", "gzip -c frutta.txt | gunzip -c | cmp - frutta.txt && echo uguale"]);
bbtest!(
    tar_roundtrip,
    [
        "sh",
        "-c",
        "mkdir d && cp frutta.txt d/ && tar cf a.tar d && rm -r d && tar xf a.tar && cat d/frutta.txt | wc -l && tar tf a.tar && rm a.tar"
    ]
);
bbtest!(
    file_ops,
    [
        "sh",
        "-c",
        "mkdir -p x/y/z && echo 1 > x/y/z/f && cp -r x w && mv w/y w/q && ln -s x/y/z/f link && cat link && rm -r x/y && ls -R w | sort"
    ]
);
bbtest!(pipeline, ["sh", "-c", "cat frutta.txt | sort | uniq | wc -l"]);
bbtest!(
    shell_script,
    [
        "sh",
        "-c",
        "f() { echo \"arg:$1\"; return 3; }; for i in 1 2 3; do f $i; done; f x; echo \"rc=$?\"; x=$((7 * 6)); [ $x -eq 42 ] && echo quarantadue; case $x in 4*) echo quattro;; esac"
    ]
);
bbtest!(
    subshell_and_redirects,
    [
        "sh",
        "-c",
        "(echo a; echo b >&2) 2>err.txt | cat; cat err.txt; echo out > o.txt; cat < o.txt; exec 3>fd3.txt; echo tre >&3; cat fd3.txt"
    ]
);
bbtest!(xargs_exec, ["sh", "-c", "printf 'uno\\ndue\\ntre\\n' | xargs -n 1 echo riga"]);
bbtest!(
    exit_codes,
    ["sh", "-c", "true; echo $?; false; echo $?; (exit 7); echo $?; sh -c 'kill -TERM $$'; echo $?"]
);
bbtest!(env_and_umask, ["sh", "-c", "export A=1; env | sort; umask 027; touch f; umask"]);
bbtest!(sleep_virtual_time, ["sh", "-c", "sleep 0.2; echo sveglio"]);
bbtest!(date_fixed, ["date", "-u", "-d", "@86400", "+%Y-%m-%d %H:%M:%S"]);
bbtest!(dc_bignum, ["dc", "-e", "2 100 ^ p"]);
