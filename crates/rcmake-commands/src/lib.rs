//! Built-in CMake 4.5.1 command families organized into disjoint submodules so parallel
//! workers can implement command families concurrently without merge conflicts.

#![forbid(unsafe_code)]

use rcmake_eval::Registry;

mod file_path;
mod policy_misc;
mod process_find;
mod string_math;
mod vars_list;

/// Register all implemented built-in CMake 4.5.1 script commands into `registry`.
pub fn register_all(registry: &mut Registry) {
    vars_list::register(registry);
    string_math::register(registry);
    file_path::register(registry);
    process_find::register(registry);
    policy_misc::register(registry);
}
