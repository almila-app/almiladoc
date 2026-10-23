//! Edge-based table assembly shared by RTF (`\cellx` boundaries) and binary
//! DOC (TAP `rgdxaCenter` boundaries): grid columns come from the clustered
//! union of every row's cell edges, horizontal merges collapse into column
//! spans, and vertical chains start at an explicit restart flag and are
//! continued by continuation cells whose boundaries match the chain's.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::{Block, Cell, GridBuilder};
use crate::shared::header::resolve_header_rows;
use std::collections::HashMap;

/// Merge/boundary properties of one cell.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CellProp {
    /// First cell of a horizontally merged set.
    pub merge_first: bool,
    /// Horizontal continuation: folds into the preceding first cell.
    pub merge_cont: bool,
    /// First cell of a vertically merged chain.
    pub vmerge_first: bool,
    /// Vertical continuation: covered by the chain's origin above.
    pub vmerge_cont: bool,
    /// The cell's right boundary in twips - vertical merge chains match on
    /// actual boundaries, not cell ordinals.
    pub right: i64,
}

type RowCells = Vec<(Vec<Block>, CellProp)>;

/// One logical row: its cells with their properties, and whether the row is
/// a header row.
#[derive(Debug)]
pub(crate) struct GridRow {
    pub cells: RowCells,
    pub header: bool,
}

/// Producer boundary jitter under this threshold clusters into one edge.
const EDGE_TOLERANCE: i64 = 10;

struct Origin {
    blocks: Vec<Block>,
    /// Half-open global column range.
    col_l: usize,
    col_r: usize,
    row_span: u32,
    vmerge_first: bool,
    covered: bool,
}

/// Each row's cells with strictly increasing right edges (cells past the declared boundaries get
/// synthetic ones).
fn normalized_edges(rows: Vec<GridRow>) -> (Vec<RowCells>, Vec<i64>) {
    let rows: Vec<RowCells> = rows
        .into_iter()
        .map(|row| {
            let mut last = i64::MIN;
            row.cells
                .into_iter()
                .map(|(blocks, mut prop)| {
                    if prop.right <= last {
                        prop.right = last + 1;
                    }
                    last = prop.right;
                    (blocks, prop)
                })
                .collect()
        })
        .collect();
    let mut clusters: Vec<i64> = rows.iter().flatten().map(|(_, prop)| prop.right).collect();
    clusters.sort_unstable();
    clusters.dedup_by(|edge, kept| *edge - *kept <= EDGE_TOLERANCE);
    (rows, clusters)
}

fn origins(rows: Vec<RowCells>, clusters: &[i64]) -> Vec<Vec<Origin>> {
    let col_of = |x: i64| clusters.partition_point(|&c| c < x - EDGE_TOLERANCE);
    rows.into_iter()
        .map(|row| {
            let mut out = Vec::new();
            let mut col = 0;
            let mut cells = row.into_iter().peekable();
            while let Some((mut blocks, prop)) = cells.next() {
                let mut right = prop.right;
                if prop.merge_first {
                    while let Some((b, p)) = cells.next_if(|(_, p)| p.merge_cont) {
                        blocks.extend(b);
                        right = p.right;
                    }
                }
                let col_r = (col_of(right) + 1).max(col + 1);
                out.push(Origin {
                    blocks,
                    col_l: col,
                    col_r,
                    row_span: 1,
                    vmerge_first: prop.vmerge_first,
                    covered: prop.vmerge_cont,
                });
                col = col_r;
            }
            out
        })
        .collect()
}

fn chain_vertical(rows: &mut [Vec<Origin>]) {
    let mut active: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
    for r in 0..rows.len() {
        let mut next_active = HashMap::new();
        for i in 0..rows[r].len() {
            let range = (rows[r][i].col_l, rows[r][i].col_r);
            if rows[r][i].covered {
                if let Some(&(orow, oidx)) = active.get(&range) {
                    rows[orow][oidx].row_span += 1;
                    next_active.insert(range, (orow, oidx));
                    continue;
                }
                rows[r][i].covered = false;
            }
            if rows[r][i].vmerge_first {
                next_active.insert(range, (r, i));
            }
        }
        active = next_active;
    }
}

/// Assemble logical rows into the canonical grid.
pub(crate) fn build_edge_table(
    parse_ctx: &ParseCtx<'_>,
    rows: Vec<GridRow>,
) -> Result<Option<Block>, ConvertError> {
    let header_rows = rows.iter().take_while(|r| r.header).count();
    let (rows, clusters) = normalized_edges(rows);
    let mut rows = origins(rows, &clusters);
    chain_vertical(&mut rows);

    let mut builder = GridBuilder::new(parse_ctx);
    for row in rows {
        builder.next_row();
        for origin in row {
            let span = u32::try_from(origin.col_r - origin.col_l).unwrap_or(u32::MAX);
            if origin.covered {
                for _ in 0..span {
                    builder.covered()?;
                }
            } else {
                builder.place(Cell::spanning(origin.blocks, span, origin.row_span))?;
            }
        }
    }
    let mut table = builder.finish()?;
    if table.grid.is_empty() {
        return Ok(None);
    }
    table.header_rows = resolve_header_rows(&table, header_rows);
    Ok(Some(Block::Table(table)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CellSlot, Inline};
    use std::assert_matches;

    fn cell(text: &str, prop: CellProp) -> (Vec<Block>, CellProp) {
        (vec![Block::Paragraph(vec![Inline::plain(text)])], prop)
    }

    #[test]
    fn horizontally_merged_cells_fold_into_their_first_cell() {
        let row = GridRow {
            cells: vec![
                cell("a", CellProp { merge_first: true, right: 1000, ..CellProp::default() }),
                cell("b", CellProp { merge_cont: true, right: 2000, ..CellProp::default() }),
                cell("c", CellProp { right: 3000, ..CellProp::default() }),
            ],
            header: false,
        };
        let Some(Block::Table(table)) = build_edge_table(&ParseCtx::default(), vec![row]).unwrap()
        else {
            panic!("a row of cells is a table")
        };
        assert_matches!(&table.grid[0][0], CellSlot::Origin(cell) if cell.col_span == 2);
        assert_matches!(table.grid[0][1], CellSlot::Covered { origin_row: 0, origin_col: 0 });
        assert_matches!(&table.grid[0][2], CellSlot::Origin(cell) if cell.col_span == 1);
    }

    fn at(right: i64) -> CellProp {
        CellProp { right, ..CellProp::default() }
    }

    fn first(right: i64) -> CellProp {
        CellProp { vmerge_first: true, ..at(right) }
    }

    fn cont(right: i64) -> CellProp {
        CellProp { vmerge_cont: true, ..at(right) }
    }

    fn rows_of(props: Vec<Vec<CellProp>>) -> Vec<GridRow> {
        let cells = |row: Vec<CellProp>| row.into_iter().map(|prop| (Vec::new(), prop)).collect();
        props.into_iter().map(|row| GridRow { cells: cells(row), header: false }).collect()
    }

    fn chained(props: Vec<Vec<CellProp>>) -> Vec<Vec<Origin>> {
        let (rows, clusters) = normalized_edges(rows_of(props));
        let mut rows = origins(rows, &clusters);
        chain_vertical(&mut rows);
        rows
    }

    #[test]
    fn an_equal_or_falling_edge_steps_up_by_one_within_its_row() {
        let (rows, _) = normalized_edges(rows_of(vec![vec![at(100), at(100), at(50)]]));
        let rights: Vec<i64> = rows[0].iter().map(|(_, prop)| prop.right).collect();
        assert_eq!(rights, [100, 101, 102]);
    }

    #[test]
    fn edges_across_rows_cluster_from_the_first_edge_within_the_tolerance() {
        for (label, rights, expected) in [
            ("a gap of exactly the tolerance joins", vec![vec![0], vec![10]], vec![0]),
            ("a gap past the tolerance starts a column", vec![vec![0], vec![11]], vec![0, 11]),
            (
                "a cluster measures from its first edge",
                vec![vec![0], vec![8], vec![16]],
                vec![0, 16],
            ),
            ("edges sort across rows", vec![vec![500], vec![100, 300]], vec![100, 300, 500]),
        ] {
            let props = rights.iter().map(|row| row.iter().map(|&right| at(right)).collect());
            assert_eq!(normalized_edges(rows_of(props.collect())).1, expected, "{label}");
        }
    }

    #[test]
    fn cells_take_the_columns_their_clustered_edges_name() {
        for (label, props, expected) in [
            (
                "edges within the tolerance share a column",
                vec![vec![at(1000), at(2000)], vec![at(1010), at(2011)]],
                vec![vec![(0, 1), (1, 2)], vec![(0, 1), (1, 3)]],
            ),
            (
                "cells in one cluster still take a column each",
                vec![vec![at(100), at(105)]],
                vec![vec![(0, 1), (1, 2)]],
            ),
        ] {
            let ranges: Vec<Vec<(usize, usize)>> = chained(props)
                .iter()
                .map(|row| row.iter().map(|origin| (origin.col_l, origin.col_r)).collect())
                .collect();
            assert_eq!(ranges, expected, "{label}");
        }
    }

    #[test]
    fn a_continuation_joins_the_chain_above_only_over_the_same_columns() {
        for (label, props, expected) in [
            (
                "a chain spans every row it continues through",
                vec![vec![first(100)], vec![cont(100)], vec![cont(100)]],
                vec![vec![(3, false)], vec![(1, true)], vec![(1, true)]],
            ),
            (
                "a continuation with no chain above shows",
                vec![vec![at(100)], vec![cont(100)]],
                vec![vec![(1, false)], vec![(1, false)]],
            ),
            (
                "a continuation over other columns shows",
                vec![vec![first(100), at(200)], vec![cont(200)]],
                vec![vec![(1, false), (1, false)], vec![(1, false)]],
            ),
            (
                "a plain row ends the chain",
                vec![vec![first(100)], vec![at(100)], vec![cont(100)]],
                vec![vec![(1, false)], vec![(1, false)], vec![(1, false)]],
            ),
            (
                "a new first starts a new chain",
                vec![vec![first(100)], vec![cont(100)], vec![first(100)], vec![cont(100)]],
                vec![vec![(2, false)], vec![(1, true)], vec![(2, false)], vec![(1, true)]],
            ),
            (
                "chains side by side stay apart",
                vec![vec![first(100), first(200)], vec![cont(100), cont(200)]],
                vec![vec![(2, false), (2, false)], vec![(1, true), (1, true)]],
            ),
        ] {
            let spans: Vec<Vec<(u32, bool)>> = chained(props)
                .iter()
                .map(|row| row.iter().map(|origin| (origin.row_span, origin.covered)).collect())
                .collect();
            assert_eq!(spans, expected, "{label}");
        }
    }
}
