use crate::support::winrar_created_case;

#[test]
fn we_read_winrar_created_plain() {
    winrar_created_case(&["-m3", "-idq"]);
}

#[test]
fn we_read_winrar_created_solid() {
    winrar_created_case(&["-m3", "-s", "-htb", "-idq"]);
}

#[test]
fn we_read_winrar_created_encrypted() {
    winrar_created_case(&["-m3", "-ppw", "-idq"]);
}

#[test]
fn we_read_winrar_created_header_encrypted() {
    winrar_created_case(&["-m3", "-ppw", "-hp", "-idq"]);
}

#[test]
fn we_read_winrar_created_multivolume() {
    winrar_created_case(&["-m0", "-v16m", "-idq"]);
}

#[test]
fn we_read_winrar_created_multivolume_encrypted() {
    winrar_created_case(&["-m0", "-v16m", "-ppw", "-idq"]);
}

#[test]
fn we_read_winrar_created_multivolume_header_encrypted() {
    winrar_created_case(&["-m0", "-v16m", "-ppw", "-hp", "-idq"]);
}
