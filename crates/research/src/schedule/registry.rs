//! Registry for managing multiple gas schedules.

use super::traits::GasSchedule;
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;

/// Errors that can occur when working with the schedule registry.
#[derive(Debug, Error)]
pub enum ScheduleError {
    /// Duplicate schedule name
    #[error("Schedule with name '{0}' already registered")]
    DuplicateName(String),

    /// Schedule not found
    #[error("Schedule '{0}' not found")]
    NotFound(String),

    /// No schedules configured
    #[error("No schedules configured - at least one schedule is required")]
    NoSchedules,

    /// Invalid schedule configuration
    #[error("Invalid schedule configuration: {0}")]
    InvalidConfig(String),
}

/// Registry for managing multiple gas schedules.
///
/// The registry holds all schedules that will be tested during execution.
/// Each schedule is identified by a unique name.
///
/// # Example
///
/// ```rust,ignore
/// use reth_research::schedule::{ScheduleRegistry, BaselineSchedule};
///
/// let mut registry = ScheduleRegistry::new();
/// registry.register(BaselineSchedule::new())?;
///
/// println!("Loaded {} schedules", registry.len());
/// ```
#[derive(Debug, Default, Clone)]
pub struct ScheduleRegistry {
    schedules: HashMap<String, Arc<dyn GasSchedule>>,
    /// Order in which schedules were registered (for deterministic iteration)
    order: Vec<String>,
}

impl ScheduleRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new schedule.
    ///
    /// # Errors
    ///
    /// Returns an error if a schedule with the same name is already registered.
    pub fn register<S: GasSchedule + 'static>(&mut self, schedule: S) -> Result<(), ScheduleError> {
        let name = schedule.name().to_string();

        if self.schedules.contains_key(&name) {
            return Err(ScheduleError::DuplicateName(name));
        }

        self.order.push(name.clone());
        self.schedules.insert(name, Arc::new(schedule));
        Ok(())
    }

    /// Register a schedule from an Arc.
    ///
    /// Useful when you already have an Arc<dyn GasSchedule>.
    pub fn register_arc(&mut self, schedule: Arc<dyn GasSchedule>) -> Result<(), ScheduleError> {
        let name = schedule.name().to_string();

        if self.schedules.contains_key(&name) {
            return Err(ScheduleError::DuplicateName(name));
        }

        self.order.push(name.clone());
        self.schedules.insert(name, schedule);
        Ok(())
    }

    /// Get a schedule by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn GasSchedule>> {
        self.schedules.get(name).cloned()
    }

    /// Get all registered schedules in registration order.
    pub fn all(&self) -> Vec<Arc<dyn GasSchedule>> {
        self.order.iter().filter_map(|name| self.schedules.get(name).cloned()).collect()
    }

    /// Get all schedule names in registration order.
    pub fn names(&self) -> &[String] {
        &self.order
    }

    /// Check if a schedule is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.schedules.contains_key(name)
    }

    /// Get the number of registered schedules.
    pub fn len(&self) -> usize {
        self.schedules.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.schedules.is_empty()
    }

    /// Validate that at least one schedule is registered.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        if self.is_empty() {
            return Err(ScheduleError::NoSchedules);
        }
        Ok(())
    }

    /// Get a summary string for logging.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "No schedules registered".to_string();
        }

        let schedule_list = self.order.join(", ");
        format!("{} schedule(s): {}", self.len(), schedule_list)
    }

    /// Get schedules that modify intrinsic gas.
    pub fn intrinsic_schedules(&self) -> Vec<Arc<dyn GasSchedule>> {
        self.all().into_iter().filter(|s| s.modifies_intrinsic()).collect()
    }

    /// Get schedules that modify execution gas.
    pub fn execution_schedules(&self) -> Vec<Arc<dyn GasSchedule>> {
        self.all().into_iter().filter(|s| s.modifies_execution()).collect()
    }

    /// Iterate over all schedules.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn GasSchedule>> {
        self.order.iter().filter_map(|name| self.schedules.get(name))
    }
}

impl<'a> IntoIterator for &'a ScheduleRegistry {
    type Item = &'a Arc<dyn GasSchedule>;
    type IntoIter = Box<dyn Iterator<Item = &'a Arc<dyn GasSchedule>> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::BaselineSchedule;

    #[derive(Debug)]
    struct TestSchedule {
        name: String,
    }

    impl GasSchedule for TestSchedule {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Test schedule"
        }

        fn kind(&self) -> super::super::ScheduleKind {
            super::super::ScheduleKind::ExecutionOnly
        }
    }

    #[test]
    fn test_registry_new() {
        let registry = ScheduleRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_registry_register() {
        let mut registry = ScheduleRegistry::new();

        registry.register(BaselineSchedule::new()).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.contains("baseline"));
    }

    #[test]
    fn test_registry_duplicate_name() {
        let mut registry = ScheduleRegistry::new();

        registry.register(BaselineSchedule::new()).unwrap();
        let result = registry.register(BaselineSchedule::new());

        assert!(matches!(result, Err(ScheduleError::DuplicateName(_))));
    }

    #[test]
    fn test_registry_get() {
        let mut registry = ScheduleRegistry::new();
        registry.register(BaselineSchedule::new()).unwrap();

        let schedule = registry.get("baseline");
        assert!(schedule.is_some());
        assert_eq!(schedule.unwrap().name(), "baseline");

        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_order_preserved() {
        let mut registry = ScheduleRegistry::new();

        registry.register(TestSchedule { name: "first".to_string() }).unwrap();
        registry.register(TestSchedule { name: "second".to_string() }).unwrap();
        registry.register(TestSchedule { name: "third".to_string() }).unwrap();

        let names = registry.names();
        assert_eq!(names, &["first", "second", "third"]);

        let all = registry.all();
        assert_eq!(all[0].name(), "first");
        assert_eq!(all[1].name(), "second");
        assert_eq!(all[2].name(), "third");
    }

    #[test]
    fn test_registry_validate() {
        let registry = ScheduleRegistry::new();
        assert!(matches!(registry.validate(), Err(ScheduleError::NoSchedules)));

        let mut registry = ScheduleRegistry::new();
        registry.register(BaselineSchedule::new()).unwrap();
        assert!(registry.validate().is_ok());
    }

    #[test]
    fn test_registry_summary() {
        let mut registry = ScheduleRegistry::new();
        assert_eq!(registry.summary(), "No schedules registered");

        registry.register(TestSchedule { name: "a".to_string() }).unwrap();
        registry.register(TestSchedule { name: "b".to_string() }).unwrap();

        assert_eq!(registry.summary(), "2 schedule(s): a, b");
    }

    #[test]
    fn test_registry_iter() {
        let mut registry = ScheduleRegistry::new();
        registry.register(TestSchedule { name: "a".to_string() }).unwrap();
        registry.register(TestSchedule { name: "b".to_string() }).unwrap();

        let names: Vec<_> = registry.iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn test_registry_into_iter() {
        let mut registry = ScheduleRegistry::new();
        registry.register(TestSchedule { name: "a".to_string() }).unwrap();
        registry.register(TestSchedule { name: "b".to_string() }).unwrap();

        let names: Vec<_> = (&registry).into_iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
