use std::{
    ffi::{CString, OsString},
    os::unix::prelude::OsStringExt,
    path::PathBuf,
};

use userland_execve::exec;

fn os_string_to_c_string(s: OsString) -> CString {
    let mut v = s.into_vec();
    v.push(0);
    CString::from_vec_with_nul(v).unwrap()
}

fn get_path() -> PathBuf {
    let path = std::env::args_os().nth(1).unwrap();
    PathBuf::from(path)
}

fn get_args() -> Vec<CString> {
    std::env::args_os()
        .skip(1)
        .map(os_string_to_c_string)
        .collect()
}

fn get_env() -> Vec<CString> {
    std::env::vars_os()
        .map(|(key, val)| {
            [key, OsString::from("="), val]
                .into_iter()
                .collect::<OsString>()
        })
        .map(os_string_to_c_string)
        .collect()
}

fn main() {
    exec(&get_path(), &get_args(), &get_env())
}
