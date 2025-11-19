use std::{
    ffi::{CStr, CString},
    iter::once,
    os::unix::prelude::OsStrExt,
    path::{Path, PathBuf},
};

use crate::stack::AuxSnapshot;

fn path_to_c_string(path: &Path) -> CString {
    CString::from_vec_with_nul(
        path.as_os_str()
            .as_bytes()
            .iter()
            .copied()
            .chain(once(0))
            .collect(),
    )
    .unwrap()
}

pub fn exec_with_options(options: ExecOptions) -> ! {
    let (bin_addr, bin_header, opt_interp) =
        crate::loader::load(&options.executable, options.interpreter);
    let path = path_to_c_string(&options.executable);
    let interp_addr = opt_interp.map(|(addr, _)| addr);
    let sp = crate::stack::make_stack(
        interp_addr,
        bin_addr,
        bin_header,
        &path,
        &options.args,
        &options.env,
        options.aux_snapshot.as_ref(),
    );
    let entry = match opt_interp {
        Some((interp_addr, interp_header)) => {
            let interp_entry: usize = interp_header.e_entry.try_into().unwrap();
            interp_addr + interp_entry
        }
        None => {
            let bin_entry: usize = bin_header.e_entry.try_into().unwrap();
            bin_addr + bin_entry
        }
    };
    unsafe { crate::run::run(sp, entry) }
}

pub fn exec(path: &Path, args: &[impl AsRef<CStr>], env: &[impl AsRef<CStr>]) -> ! {
    let mut options = ExecOptions::new(path);
    options.args(args);
    options.env_pairs(env);

    exec_with_options(options)
}

pub struct ExecOptions {
    pub executable: PathBuf,
    pub args: Vec<CString>,
    pub env: Vec<CString>,
    interpreter: crate::loader::Interpreter,
    aux_snapshot: Option<AuxSnapshot>,
}

impl ExecOptions {
    pub fn new(executable: impl AsRef<Path>) -> Self {
        Self {
            executable: executable.as_ref().to_owned(),
            args: vec![],
            env: vec![],
            interpreter: crate::loader::Interpreter::FromHeader,
            aux_snapshot: None,
        }
    }

    pub fn arg(&mut self, value: impl AsRef<CStr>) -> &mut Self {
        self.args.push(value.as_ref().to_owned());
        self
    }

    pub fn args(&mut self, values: impl IntoIterator<Item = impl AsRef<CStr>>) -> &mut Self {
        self.args
            .extend(values.into_iter().map(|v| v.as_ref().to_owned()));
        self
    }

    pub fn env(&mut self, key: impl AsRef<CStr>, value: impl AsRef<CStr>) -> &mut Self {
        let mut pair = key.as_ref().to_bytes().to_vec();
        pair.push(b'=');
        pair.extend(value.as_ref().to_bytes());
        self.env.push(CString::new(pair).unwrap());
        self
    }

    pub fn env_pairs(&mut self, pairs: impl IntoIterator<Item = impl AsRef<CStr>>) -> &mut Self {
        self.env
            .extend(pairs.into_iter().map(|v| v.as_ref().to_owned()));
        self
    }

    pub fn envs(
        &mut self,
        pairs: impl IntoIterator<Item = (impl AsRef<CStr>, impl AsRef<CStr>)>,
    ) -> &mut Self {
        for (key, value) in pairs {
            self.env(key, value);
        }
        self
    }

    pub fn override_interpreter(&mut self, interpreter: Option<impl AsRef<Path>>) -> &mut Self {
        self.interpreter = match interpreter {
            Some(path) => crate::loader::Interpreter::Path(path.as_ref().to_owned()),
            None => crate::loader::Interpreter::None,
        };
        self
    }

    pub fn aux_snapshot(&mut self, snapshot: AuxSnapshot) -> &mut Self {
        self.aux_snapshot = Some(snapshot);
        self
    }
}
