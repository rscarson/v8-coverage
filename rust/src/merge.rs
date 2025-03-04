use crate::coverage::{FunctionCov, ProcessCov, RangeCov, ScriptCov};
use crate::normalize::{deep_normalize_proces_cov, deep_normalize_script_cov, normalize_function_cov};
use crate::range_tree::RangeTree;
use crate::range_tree::RangeTreeArena;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::iter::Peekable;

pub fn merge_processes(mut processes: Vec<ProcessCov>) -> Option<ProcessCov> {
  if processes.len() <= 1 {
    return processes.pop().map(|mut cov| {
      deep_normalize_proces_cov(&mut cov);
      cov
    });
  }
  let mut url_to_scripts: BTreeMap<String, Vec<ScriptCov>> = BTreeMap::new();
  for process_cov in processes {
    for script_cov in process_cov.result {
      url_to_scripts
        .entry(script_cov.url.clone())
        .or_insert(Vec::new())
        .push(script_cov);
    }
  }

  let result: Vec<(usize, Vec<ScriptCov>)> = url_to_scripts
    .into_iter()
    .enumerate()
    .map(|(script_id, (_, scripts))| (script_id, scripts))
    .collect();

  let result: Vec<ScriptCov> = result
    //    .into_par_iter()
    .par_iter()
    //    .into_iter()
    .map(|(script_id, scripts)| {
      let mut merged: ScriptCov = merge_scripts(scripts.to_vec()).unwrap();
      merged.script_id = script_id.to_string();
      merged
    })
    .collect();

  Some(ProcessCov { result })
}

pub fn merge_scripts(mut scripts: Vec<ScriptCov>) -> Option<ScriptCov> {
  if scripts.len() <= 1 {
    return scripts.pop().map(|mut cov| {
      deep_normalize_script_cov(&mut cov);
      cov
    });
  }
  let (script_id, url) = {
    let first: &ScriptCov = &scripts[0];
    (first.script_id.clone(), first.url.clone())
  };
  let mut range_to_funcs: BTreeMap<Range, Vec<FunctionCov>> = BTreeMap::new();
  for script_cov in scripts {
    for func_cov in script_cov.functions {
      let root_range = {
        let root_range_cov: &RangeCov = &func_cov.ranges[0];
        Range {
          start: root_range_cov.start_offset,
          end: root_range_cov.end_offset,
        }
      };
      range_to_funcs.entry(root_range).or_insert(Vec::new()).push(func_cov);
    }
  }

  let functions: Vec<FunctionCov> = range_to_funcs
    .into_iter()
    .map(|(_, funcs)| merge_functions(funcs).unwrap())
    .collect();

  Some(ScriptCov {
    script_id,
    url,
    functions,
  })
}

#[derive(Eq, PartialEq, Hash, Copy, Clone, Debug)]
struct Range {
  start: usize,
  end: usize,
}

impl Ord for Range {
  fn cmp(&self, other: &Self) -> ::std::cmp::Ordering {
    if self.start != other.start {
      self.start.cmp(&other.start)
    } else {
      other.end.cmp(&self.end)
    }
  }
}

impl PartialOrd for Range {
  fn partial_cmp(&self, other: &Self) -> Option<::std::cmp::Ordering> {
    if self.start != other.start {
      self.start.partial_cmp(&other.start)
    } else {
      other.end.partial_cmp(&self.end)
    }
  }
}

pub fn merge_functions(mut funcs: Vec<FunctionCov>) -> Option<FunctionCov> {
  if funcs.len() <= 1 {
    return funcs.pop().map(|mut cov| {
      normalize_function_cov(&mut cov);
      cov
    });
  }
  let function_name = funcs[0].function_name.clone();
  let rta_capacity: usize = funcs.iter().fold(0, |acc, func| acc + func.ranges.len());
  let rta = RangeTreeArena::with_capacity(rta_capacity);
  let mut trees: Vec<&mut RangeTree> = Vec::new();
  for func in funcs {
    if let Some(tree) = RangeTree::from_sorted_ranges(&rta, &func.ranges) {
      trees.push(tree);
    }
  }
  let merged = RangeTree::normalize(&rta, merge_range_trees(&rta, trees).unwrap());
  let ranges = merged.to_ranges();
  let is_block_coverage: bool = !(ranges.len() == 1 && ranges[0].count == 0);

  Some(FunctionCov {
    function_name,
    ranges,
    is_block_coverage,
  })
}

fn merge_range_trees<'a>(
  rta: &'a RangeTreeArena<'a>,
  mut trees: Vec<&'a mut RangeTree<'a>>,
) -> Option<&'a mut RangeTree<'a>> {
  if trees.len() <= 1 {
    return trees.pop();
  }
  let (start, end) = {
    let first = &trees[0];
    (first.start, first.end)
  };
  let delta: i64 = trees.iter().fold(0, |acc, tree| acc + tree.delta);
  let children = merge_range_tree_children(rta, trees);

  Some(rta.alloc(RangeTree::new(start, end, delta, children)))
}

struct StartEvent<'a> {
  offset: usize,
  trees: Vec<(usize, &'a mut RangeTree<'a>)>,
}

fn into_start_events<'a>(trees: Vec<&'a mut RangeTree<'a>>) -> Vec<StartEvent> {
  let mut result: BTreeMap<usize, Vec<(usize, &'a mut RangeTree<'a>)>> = BTreeMap::new();
  for (parent_index, tree) in trees.into_iter().enumerate() {
    for child in tree.children.drain(..) {
      result
        .entry(child.start)
        .or_insert(Vec::new())
        .push((parent_index, child));
    }
  }
  result
    .into_iter()
    .map(|(offset, trees)| StartEvent { offset, trees })
    .collect()
}

struct StartEventQueue<'a> {
  pending: Option<StartEvent<'a>>,
  queue: Peekable<::std::vec::IntoIter<StartEvent<'a>>>,
}

impl<'a> StartEventQueue<'a> {
  pub fn new(queue: Vec<StartEvent<'a>>) -> StartEventQueue<'a> {
    StartEventQueue {
      pending: None,
      queue: queue.into_iter().peekable(),
    }
  }

  pub(crate) fn set_pending_offset(&mut self, offset: usize) -> () {
    self.pending = Some(StartEvent {
      offset,
      trees: Vec::new(),
    });
  }

  pub(crate) fn push_pending_tree(&mut self, tree: (usize, &'a mut RangeTree<'a>)) -> () {
    self.pending = self.pending.take().map(|mut start_event| {
      start_event.trees.push(tree);
      start_event
    });
  }
}

impl<'a> Iterator for StartEventQueue<'a> {
  type Item = StartEvent<'a>;

  fn next(&mut self) -> Option<<Self as Iterator>::Item> {
    let pending_offset: Option<usize> = match &self.pending {
      Some(ref start_event) if !start_event.trees.is_empty() => Some(start_event.offset),
      _ => None,
    };

    match pending_offset {
      Some(pending_offset) => {
        let queue_offset = self.queue.peek().map(|start_event| start_event.offset);
        match queue_offset {
          None => self.pending.take(),
          Some(queue_offset) => {
            if pending_offset < queue_offset {
              self.pending.take()
            } else {
              let mut result = self.queue.next().unwrap();
              if pending_offset == queue_offset {
                let pending_trees = self.pending.take().unwrap().trees;
                result.trees.extend(pending_trees.into_iter())
              }
              Some(result)
            }
          }
        }
      }
      None => self.queue.next(),
    }
  }
}

fn merge_range_tree_children<'a>(
  rta: &'a RangeTreeArena<'a>,
  parent_trees: Vec<&'a mut RangeTree<'a>>,
) -> Vec<&'a mut RangeTree<'a>> {
  let mut flat_children: Vec<Vec<&'a mut RangeTree<'a>>> = Vec::with_capacity(parent_trees.len());
  let mut wrapped_children: Vec<Vec<&'a mut RangeTree<'a>>> = Vec::with_capacity(parent_trees.len());
  let mut open_range: Option<Range> = None;

  for parent_tree in parent_trees.iter() {
    flat_children.push(Vec::new());
    wrapped_children.push(Vec::new());
  }

  let mut start_event_queue = StartEventQueue::new(into_start_events(parent_trees));

  let mut parent_to_nested: HashMap<usize, Vec<&'a mut RangeTree<'a>>> = HashMap::new();

  while let Some(event) = start_event_queue.next() {
    open_range = if let Some(open_range) = open_range {
      if open_range.end <= event.offset {
        for (parent_index, nested) in parent_to_nested {
          wrapped_children[parent_index].push(rta.alloc(RangeTree::new(open_range.start, open_range.end, 0, nested)));
        }
        parent_to_nested = HashMap::new();
        None
      } else {
        Some(open_range)
      }
    } else {
      None
    };

    match open_range {
      Some(open_range) => {
        for (parent_index, mut tree) in event.trees {
          let child = if tree.end > open_range.end {
            let (left, right) = RangeTree::split(rta, tree, open_range.end);
            start_event_queue.push_pending_tree((parent_index, right));
            left
          } else {
            tree
          };
          parent_to_nested.entry(parent_index).or_insert(Vec::new()).push(child);
        }
      }
      None => {
        let mut open_range_end: usize = event.offset + 1;
        for (_, ref tree) in &event.trees {
          open_range_end = if tree.end > open_range_end {
            tree.end
          } else {
            open_range_end
          };
        }
        for (parent_index, tree) in event.trees {
          if tree.end == open_range_end {
            flat_children[parent_index].push(tree);
            continue;
          }
          parent_to_nested.entry(parent_index).or_insert(Vec::new()).push(tree);
        }
        start_event_queue.set_pending_offset(open_range_end);
        open_range = Some(Range {
          start: event.offset,
          end: open_range_end,
        });
      }
    }
  }
  if let Some(open_range) = open_range {
    for (parent_index, nested) in parent_to_nested {
      wrapped_children[parent_index].push(rta.alloc(RangeTree::new(open_range.start, open_range.end, 0, nested)));
    }
  }

  let child_forests: Vec<Vec<&'a mut RangeTree<'a>>> = flat_children
    .into_iter()
    .zip(wrapped_children.into_iter())
    .map(|(flat, wrapped)| merge_children_lists(flat, wrapped))
    .collect();

  let events = get_child_events_from_forests(&child_forests);

  let mut child_forests: Vec<Peekable<::std::vec::IntoIter<&'a mut RangeTree<'a>>>> = child_forests
    .into_iter()
    .map(|forest| forest.into_iter().peekable())
    .collect();

  let mut result: Vec<&'a mut RangeTree<'a>> = Vec::new();
  for event in events.iter() {
    let mut matching_trees: Vec<&'a mut RangeTree<'a>> = Vec::new();
    for (parent_index, children) in child_forests.iter_mut().enumerate() {
      let next_tree: Option<&'a mut RangeTree<'a>> = {
        if children.peek().map_or(false, |tree| tree.start == *event) {
          children.next()
        } else {
          None
        }
      };
      if let Some(next_tree) = next_tree {
        matching_trees.push(next_tree);
      }
    }
    if let Some(mut merged) = merge_range_trees(rta, matching_trees) {
      result.push(merged);
    }
  }

  result
}

fn get_child_events_from_forests<'a>(forests: &Vec<Vec<&'a mut RangeTree<'a>>>) -> BTreeSet<usize> {
  let mut event_set: BTreeSet<usize> = BTreeSet::new();
  for forest in forests {
    for tree in forest {
      event_set.insert(tree.start);
      event_set.insert(tree.end);
    }
  }
  event_set
}

// TODO: itertools?
// https://play.integer32.com/?gist=ad2cd20d628e647a5dbdd82e68a15cb6&version=stable&mode=debug&edition=2015
fn merge_children_lists<'a>(
  a: Vec<&'a mut RangeTree<'a>>,
  b: Vec<&'a mut RangeTree<'a>>,
) -> Vec<&'a mut RangeTree<'a>> {
  let mut merged: Vec<&'a mut RangeTree<'a>> = Vec::new();
  let mut a = a.into_iter();
  let mut b = b.into_iter();
  let mut next_a = a.next();
  let mut next_b = b.next();
  loop {
    match (next_a, next_b) {
      (Some(tree_a), Some(tree_b)) => {
        if tree_a.start < tree_b.start {
          merged.push(tree_a);
          next_a = a.next();
          next_b = Some(tree_b);
        } else {
          merged.push(tree_b);
          next_a = Some(tree_a);
          next_b = b.next();
        }
      }
      (Some(tree_a), None) => {
        merged.push(tree_a);
        merged.extend(a);
        break;
      }
      (None, Some(tree_b)) => {
        merged.push(tree_b);
        merged.extend(b);
        break;
      }
      (None, None) => break,
    }
  }

  merged
}

#[cfg(test)]
mod tests {
  use super::merge_processes;
  use crate::coverage::{FunctionCov, ProcessCov, RangeCov, ScriptCov};
  use test_generator::test_resources;

  #[test]
  fn empty() {
    let inputs: Vec<ProcessCov> = Vec::new();
    let expected: Option<ProcessCov> = None;

    assert_eq!(merge_processes(inputs), expected);
  }

  #[test]
  fn two_flat_trees() {
    let inputs: Vec<ProcessCov> = vec![
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![RangeCov {
              start_offset: 0,
              end_offset: 9,
              count: 1,
            }],
          }],
        }],
      },
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![RangeCov {
              start_offset: 0,
              end_offset: 9,
              count: 2,
            }],
          }],
        }],
      },
    ];
    let expected: Option<ProcessCov> = Some(ProcessCov {
      result: vec![ScriptCov {
        script_id: String::from("0"),
        url: String::from("/lib.js"),
        functions: vec![FunctionCov {
          function_name: String::from("lib"),
          is_block_coverage: true,
          ranges: vec![RangeCov {
            start_offset: 0,
            end_offset: 9,
            count: 3,
          }],
        }],
      }],
    });

    assert_eq!(merge_processes(inputs), expected);
  }

  #[test]
  fn two_trees_with_matching_children() {
    let inputs: Vec<ProcessCov> = vec![
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 9,
                count: 10,
              },
              RangeCov {
                start_offset: 3,
                end_offset: 6,
                count: 1,
              },
            ],
          }],
        }],
      },
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 9,
                count: 20,
              },
              RangeCov {
                start_offset: 3,
                end_offset: 6,
                count: 2,
              },
            ],
          }],
        }],
      },
    ];
    let expected: Option<ProcessCov> = Some(ProcessCov {
      result: vec![ScriptCov {
        script_id: String::from("0"),
        url: String::from("/lib.js"),
        functions: vec![FunctionCov {
          function_name: String::from("lib"),
          is_block_coverage: true,
          ranges: vec![
            RangeCov {
              start_offset: 0,
              end_offset: 9,
              count: 30,
            },
            RangeCov {
              start_offset: 3,
              end_offset: 6,
              count: 3,
            },
          ],
        }],
      }],
    });

    assert_eq!(merge_processes(inputs), expected);
  }

  #[test]
  fn two_trees_with_partially_overlapping_children() {
    let inputs: Vec<ProcessCov> = vec![
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 9,
                count: 10,
              },
              RangeCov {
                start_offset: 2,
                end_offset: 5,
                count: 1,
              },
            ],
          }],
        }],
      },
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 9,
                count: 20,
              },
              RangeCov {
                start_offset: 4,
                end_offset: 7,
                count: 2,
              },
            ],
          }],
        }],
      },
    ];
    let expected: Option<ProcessCov> = Some(ProcessCov {
      result: vec![ScriptCov {
        script_id: String::from("0"),
        url: String::from("/lib.js"),
        functions: vec![FunctionCov {
          function_name: String::from("lib"),
          is_block_coverage: true,
          ranges: vec![
            RangeCov {
              start_offset: 0,
              end_offset: 9,
              count: 30,
            },
            RangeCov {
              start_offset: 2,
              end_offset: 5,
              count: 21,
            },
            RangeCov {
              start_offset: 4,
              end_offset: 5,
              count: 3,
            },
            RangeCov {
              start_offset: 5,
              end_offset: 7,
              count: 12,
            },
          ],
        }],
      }],
    });

    assert_eq!(merge_processes(inputs), expected);
  }

  #[test]
  fn two_trees_with_with_complementary_children_summing_to_the_same_count() {
    let inputs: Vec<ProcessCov> = vec![
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 9,
                count: 1,
              },
              RangeCov {
                start_offset: 1,
                end_offset: 8,
                count: 6,
              },
              RangeCov {
                start_offset: 1,
                end_offset: 5,
                count: 5,
              },
              RangeCov {
                start_offset: 5,
                end_offset: 8,
                count: 7,
              },
            ],
          }],
        }],
      },
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 9,
                count: 4,
              },
              RangeCov {
                start_offset: 1,
                end_offset: 8,
                count: 8,
              },
              RangeCov {
                start_offset: 1,
                end_offset: 5,
                count: 9,
              },
              RangeCov {
                start_offset: 5,
                end_offset: 8,
                count: 7,
              },
            ],
          }],
        }],
      },
    ];
    let expected: Option<ProcessCov> = Some(ProcessCov {
      result: vec![ScriptCov {
        script_id: String::from("0"),
        url: String::from("/lib.js"),
        functions: vec![FunctionCov {
          function_name: String::from("lib"),
          is_block_coverage: true,
          ranges: vec![
            RangeCov {
              start_offset: 0,
              end_offset: 9,
              count: 5,
            },
            RangeCov {
              start_offset: 1,
              end_offset: 8,
              count: 14,
            },
          ],
        }],
      }],
    });

    assert_eq!(merge_processes(inputs), expected);
  }

  #[test]
  fn merges_a_similar_sliding_chain_a_bc() {
    let inputs: Vec<ProcessCov> = vec![
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 7,
                count: 10,
              },
              RangeCov {
                start_offset: 0,
                end_offset: 4,
                count: 1,
              },
            ],
          }],
        }],
      },
      ProcessCov {
        result: vec![ScriptCov {
          script_id: String::from("0"),
          url: String::from("/lib.js"),
          functions: vec![FunctionCov {
            function_name: String::from("lib"),
            is_block_coverage: true,
            ranges: vec![
              RangeCov {
                start_offset: 0,
                end_offset: 7,
                count: 20,
              },
              RangeCov {
                start_offset: 1,
                end_offset: 6,
                count: 11,
              },
              RangeCov {
                start_offset: 2,
                end_offset: 5,
                count: 2,
              },
            ],
          }],
        }],
      },
    ];
    let expected: Option<ProcessCov> = Some(ProcessCov {
      result: vec![ScriptCov {
        script_id: String::from("0"),
        url: String::from("/lib.js"),
        functions: vec![FunctionCov {
          function_name: String::from("lib"),
          is_block_coverage: true,
          ranges: vec![
            RangeCov {
              start_offset: 0,
              end_offset: 7,
              count: 30,
            },
            RangeCov {
              start_offset: 0,
              end_offset: 6,
              count: 21,
            },
            RangeCov {
              start_offset: 1,
              end_offset: 5,
              count: 12,
            },
            RangeCov {
              start_offset: 2,
              end_offset: 4,
              count: 3,
            },
          ],
        }],
      }],
    });

    assert_eq!(merge_processes(inputs), expected);
  }

  fn is_test_blacklisted(test_name: &str) -> bool {
    match test_name {
      "is-block-coverage" => true,
      "issue-2-mixed-is-block-coverage" => true,
      "node-10.11.0" => true,
      "npm-6.4.1" => true,
      "yargs-12.0.2" => true,
      _ => false,
    }
  }

  #[test_resources("./tests/merge/*/")]
  fn test_merge(path: &str) -> () {
    use std::path::{Path, PathBuf};
    let path: PathBuf = Path::join(Path::new(".."), path);
    let name = path
      .components()
      .last()
      .unwrap()
      .as_os_str()
      .to_str()
      .expect("Failed to retrieve test name");

    if is_test_blacklisted(&name) {
      eprintln!("Skipping blacklisted test");
      return;
    }

    let test_path = path.join("test.json");

    let test_json = ::std::fs::read_to_string(test_path).expect("Failed to read test file");

    let test: Vec<MergeTestItem> = serde_json::from_str(&test_json).expect("Failed to read test");

    for item in test {
      assert_eq!(merge_processes(item.inputs).unwrap(), item.expected);
    }
  }

  use serde::{Deserialize, Serialize};

  #[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct MergeTestItem {
    pub inputs: Vec<ProcessCov>,
    pub expected: ProcessCov,
  }
}
