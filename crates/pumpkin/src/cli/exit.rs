//! How `pumpkin` and `pumpkin-admin` end: [`Exit`] is the status that `main` returns. A help says
//! what each one means.

use std::process::{ExitCode, Termination};

// Defines `Exit`, each status with its code, and `ALL`, the statuses in the order listed, as
// `lang`'s `tokens!` defines its tokens. The check of the help finds each of `ALL` in a help. The
// codes are the discriminants, so no two statuses share one.
macro_rules! statuses {
    ($($status:ident = $code:literal,)*) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        #[repr(u8)]
        pub(crate) enum Exit {
            $($status = $code,)*
        }

        impl Exit {
            pub(super) const ALL: &[Self] = &[$(Self::$status,)*];
        }
    };
}

statuses! {
    Success = 0,
    Failure = 1,
    Usage = 2,
    Partial = 3,
}

impl Exit {
    pub(super) const fn code(self) -> u8 {
        self as u8
    }
}

impl Termination for Exit {
    fn report(self) -> ExitCode {
        ExitCode::from(self.code())
    }
}
