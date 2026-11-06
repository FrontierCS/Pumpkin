//! The FrontierCS factory, for tasks ported from FrontierCS to Pumpkin as data, each in
//! `tasks/frontier-cs/<problem id>/`: a [`Task`] is one, as [`Task::load`] takes it from its
//! directory.

mod task;

pub use task::Task;
