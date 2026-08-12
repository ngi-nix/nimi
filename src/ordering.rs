//! Ordering module extracted from rustysd
//!
//! Provides service dependency ordering, cycle detection, and startup graph collection.
//! Based on rustysd's ordering system: https://github.com/KillingSpark/rustysd

use std::collections::HashMap;

/// Service identifier
#[derive(Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
pub struct UnitId {
    /// Service name
    pub name: String,
}

impl UnitId {
    /// Create a new UnitId
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl PartialEq<str> for UnitId {
    fn eq(&self, other: &str) -> bool {
        self.name == other
    }
}

impl PartialEq<String> for UnitId {
    fn eq(&self, other: &String) -> bool {
        self.name == *other
    }
}

/// Service dependencies
#[derive(Debug, Clone, Default)]
pub struct Dependencies {
    /// Services that should start before this one
    pub before: Vec<UnitId>,
    /// Services this service should start after
    pub after: Vec<UnitId>,
}

impl Dependencies {
    /// Get units that must start before this unit
    pub fn start_before_this(&self) -> Vec<UnitId> {
        self.after.clone()
    }

    /// Deduplicate dependencies
    pub fn dedup(&mut self) {
        self.before.sort();
        self.after.sort();
        self.before.dedup();
        self.after.dedup();
    }

    fn remove_from_vec(ids: &mut Vec<UnitId>, id: &UnitId) {
        while let Some(idx) = ids.iter().position(|e| e == id) {
            ids.remove(idx);
        }
    }

    /// Remove a unit from all dependency lists
    pub fn remove_id(&mut self, id: &UnitId) {
        Self::remove_from_vec(&mut self.before, id);
        Self::remove_from_vec(&mut self.after, id);
    }

    /// Check if this service comes after the named service
    pub fn comes_after(&self, name: &str) -> bool {
        self.after.iter().any(|id| id.name == name)
    }

    /// Check if this service comes before the named service
    pub fn comes_before(&self, name: &str) -> bool {
        self.before.iter().any(|id| id.name == name)
    }
}

/// Errors from dependency validation
#[derive(Debug, Eq, PartialEq)]
pub enum SanityCheckError {
    /// Generic error message
    Generic(String),
    /// Cycles detected in the dependency graph
    CirclesFound(Vec<Vec<UnitId>>),
}

impl std::fmt::Display for SanityCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SanityCheckError::Generic(msg) => write!(f, "{}", msg),
            SanityCheckError::CirclesFound(circles) => {
                write!(f, "Cycles found: ")?;
                for circle in circles {
                    write!(f, "{:?}", circle)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SanityCheckError {}

/// Validate that the unit dependencies form a valid DAG (no cycles)
///
/// Uses Kahn's algorithm for topological sorting
pub fn sanity_check_dependencies(
    unit_table: &HashMap<UnitId, Dependencies>,
) -> Result<(), SanityCheckError> {
    let mut root_ids = Vec::new();
    for unit in unit_table.values() {
        if unit.after.is_empty() {
            root_ids.push(unit.after.clone());
        }
    }

    let mut finished_ids = HashMap::new();
    let mut not_finished_ids: HashMap<_, _> =
        unit_table.keys().map(|id| (id.clone(), ())).collect();
    let mut circles = Vec::new();

    loop {
        let root_id = if not_finished_ids.is_empty() {
            break;
        } else {
            let root_id = not_finished_ids.keys().find(|id| {
                let unit = unit_table.get(id).unwrap();
                let in_degree = unit.after.iter().fold(0, |acc, id| {
                    if finished_ids.contains_key(id) {
                        acc
                    } else {
                        acc + 1
                    }
                });
                in_degree == 0
            });
            if let Some(id) = root_id {
                id.clone()
            } else {
                circles.push(not_finished_ids.keys().cloned().collect());
                break;
            }
        };

        let mut visited_ids = Vec::new();
        if let Err(SanityCheckError::CirclesFound(new_circles)) = search_backedge(
            &root_id,
            unit_table,
            &mut visited_ids,
            &mut finished_ids,
            &mut not_finished_ids,
        ) {
            circles.extend(new_circles)
        };
    }

    if circles.is_empty() {
        Ok(())
    } else {
        Err(SanityCheckError::CirclesFound(circles))
    }
}

fn search_backedge(
    id: &UnitId,
    unit_table: &HashMap<UnitId, Dependencies>,
    visited_ids: &mut Vec<UnitId>,
    finished_ids: &mut HashMap<UnitId, ()>,
    not_finished_ids: &mut HashMap<UnitId, ()>,
) -> Result<(), SanityCheckError> {
    if finished_ids.contains_key(id) {
        return Ok(());
    }

    if visited_ids.contains(id) {
        let circle_start_idx = visited_ids.iter().position(|i| i == id).unwrap_or(0);
        let circle_ids = visited_ids[circle_start_idx..].to_vec();
        for circleid in &circle_ids {
            finished_ids.insert(circleid.clone(), ());
            not_finished_ids.remove(circleid);
        }
        return Err(SanityCheckError::CirclesFound(vec![circle_ids]));
    }
    visited_ids.push(id.clone());

    let unit = unit_table.get(id).unwrap();
    for next_id in &unit.before {
        let res = search_backedge(
            next_id,
            unit_table,
            visited_ids,
            finished_ids,
            not_finished_ids,
        );
        res?;
    }
    visited_ids.pop();
    finished_ids.insert(id.clone(), ());
    not_finished_ids.remove(id);

    Ok(())
}

/// Collect all units that need to be started to reach the target units
///
/// Extends ids_to_start with all dependencies recursively
pub fn collect_unit_start_subgraph(
    ids_to_start: &mut Vec<UnitId>,
    unit_table: &HashMap<UnitId, Dependencies>,
) {
    loop {
        let mut new_ids = Vec::new();
        for id in ids_to_start.iter() {
            let unit = match unit_table.get(id) {
                Some(u) => u,
                None => continue,
            };
            new_ids.extend(unit.start_before_this());
        }
        new_ids.sort();
        new_ids.dedup();

        // keep only ids not already in ids_to_start
        let already_present: Vec<UnitId> = ids_to_start.to_vec();
        new_ids.retain(|id| !already_present.contains(id));

        if new_ids.is_empty() {
            break;
        } else {
            ids_to_start.extend(new_ids);
        }
    }
}

/// Find units that can be started given current started state
pub fn find_startable_units(
    ids: &[UnitId],
    unit_table: &HashMap<UnitId, Dependencies>,
    started: &HashMap<UnitId, bool>,
) -> Vec<UnitId> {
    let mut startable = Vec::new();

    for id in ids {
        let unit = match unit_table.get(id) {
            Some(u) => u,
            None => continue,
        };

        let all_deps_satisfied = unit
            .after
            .iter()
            .all(|dep| started.get(dep).copied().unwrap_or(false));

        if all_deps_satisfied {
            startable.push(id.clone());
        }
    }
    startable
}

/// Add reverse dependency edges (before, after)
///
/// This ensures bidirectional tracking of dependencies
pub fn fill_dependencies(
    unit_table: &mut HashMap<UnitId, Dependencies>,
) -> Result<(), SanityCheckError> {
    let mut before = Vec::new();
    let mut after = Vec::new();

    for (id, deps) in unit_table.iter() {
        for before_id in &deps.before {
            after.push((id.clone(), before_id.clone()));
        }
        for after_id in &deps.after {
            before.push((id.clone(), after_id.clone()));
        }
    }

    for (before_id, after_id) in before {
        if let Some(unit) = unit_table.get_mut(&after_id) {
            unit.before.push(before_id);
        }
    }

    for (after_id, before_id) in after {
        if let Some(unit) = unit_table.get_mut(&before_id) {
            unit.after.push(after_id);
        }
    }

    for unit in unit_table.values_mut() {
        unit.dedup();
    }

    Ok(())
}

/// Find services that should be stopped when a service fails.
///
/// Since `after` is soft ordering (no failure propagation), this returns an empty list.
/// Kept for future use if hard dependencies are reintroduced.
pub fn propagate_failure(
    _failed_service: &str,
    _unit_table: &HashMap<UnitId, Dependencies>,
) -> Vec<String> {
    Vec::new()
}
