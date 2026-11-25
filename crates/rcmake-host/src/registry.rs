//! Windows registry queries used by search commands.

/// A predefined Windows registry hive.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RegistryRoot {
    /// HKEY_CURRENT_USER.
    CurrentUser,
    /// HKEY_LOCAL_MACHINE.
    LocalMachine,
    /// HKEY_CLASSES_ROOT.
    ClassesRoot,
    /// HKEY_CURRENT_CONFIG.
    CurrentConfig,
    /// HKEY_USERS.
    Users,
}

/// The registry view used for a query.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RegistryView {
    /// The 32-bit registry view.
    Bits32,
    /// The 64-bit registry view.
    Bits64,
}
