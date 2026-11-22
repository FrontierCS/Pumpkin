//! The files of a Pumpkin install: `pumpkin` and the files beside it, in one directory, `bin/`. A
//! release archives an install, and a controller installs it whole. `cargo xtask build` lays the
//! install out. `pumpkin` sends its own install to a controller.
#![no_std]

/// Declares `File`, `File::ALL` and each file's name and kind from the one table that follows. So
/// a file added to the table is in all of them. A kind is `executable`, a static executable, or
/// `data`, which other programs read.
macro_rules! files {
    (@is_executable executable) => {
        true
    };
    (@is_executable data) => {
        false
    };
    ($($(#[$doc:meta])* $file:ident = $name:literal, $kind:ident;)*) => {
        /// A file of an install, in its `bin/`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum File {
            $($(#[$doc])* $file,)*
        }

        impl File {
            /// Every file of an install, `pumpkin` first.
            pub const ALL: [Self; [$(Self::$file),*].len()] = [$(Self::$file),*];

            /// Its name in `bin/`, by which `pumpkin` finds it beside itself.
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$file => $name,)*
                }
            }

            /// Whether it is a static executable, which an install keeps executable. Other programs
            /// read the rest.
            pub const fn is_executable(self) -> bool {
                match self {
                    $(Self::$file => files!(@is_executable $kind),)*
                }
            }
        }
    };
}

files! {
    Pumpkin = "pumpkin", executable;
    Buildifier = "buildifier", executable;
    Ec2Boot = "ec2-boot", executable;
    Sandboxd = "sandboxd", executable;
    /// The Modal provider's helper, a script that `uv` runs.
    ModalHelper = "pumpkin-modal.py", data;
    // `uv` names the lock after the helper, with `.lock` appended.
    /// The lock of the helper's packages.
    ModalLock = "pumpkin-modal.py.lock", data;
}
