//! 强类型标识符，避免各类 ID 混用。

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// 随机生成一个新 ID。
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// 短形式（前 8 位，日志友好）。
            pub fn short(&self) -> String {
                self.0.simple().to_string()[..8].to_string()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.short())
            }
        }
    };
}

id_type!(BuildId, "bld");
id_type!(ProjectId, "prj");
