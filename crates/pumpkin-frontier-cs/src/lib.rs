//! The FrontierCS factory, for tasks ported from FrontierCS to Pumpkin as data, each in
//! `tasks/frontier-cs/<problem id>/`: a [`Task`] is one, as [`Task::load`] takes it from its
//! directory, and [`Task::build`] builds its images. The `pumpkin-frontier-cs` command does
//! both.

mod images;
mod task;

pub use images::Images;
pub use task::Task;
