// ---------------------------------------------------------------------------
// Returning blocks that end in the same releases share one copy of them.
// ---------------------------------------------------------------------------

/// What a release statement does: the call and what it is handed, spelled
/// out. The unit it answers into is left out, so two exits that give back the
/// same place compare equal.
fn exit_release_key(stmt: &Statement, mentions: &[u32]) -> Option<String> {
    let StatementKind::Assign {
        place,
        rvalue: Rvalue::CallIntrinsic { name, args },
    } = &stmt.kind
    else {
        return None;
    };
    let answer_unread = place.projection.is_empty()
        && mentions
            .get(place.local.0 as usize)
            .is_some_and(|&count| count == 1);
    (answer_unread && crate::uniqueness::is_release_intrinsic(name))
        .then(|| format!("{name}{args:?}"))
}

/// The releases a returning block ends with, split off from it.
struct ExitRun {
    block: usize,
    /// The releases with their keys, in the order the block runs them.
    run: Vec<(String, Statement)>,
    /// The run's positions in the order the shared tails use: releases more
    /// exits make come later.
    order: Vec<usize>,
}

/// A node of the trie of exit tails: the release it adds before the tail of
/// its parent, which runs nearer the return.
struct TailNode {
    parent: Option<usize>,
    depth: usize,
    /// The exit and run position of the release it stands for.
    stmt: (usize, usize),
    exits: usize,
}

/// Gives returning blocks whose trailing releases are the same one shared
/// copy of them. A function with many exits gives back every live value at
/// each, so its size otherwise grows as its exits times its live values.
///
/// Each release in such a run gives back a share the frame owns and reads
/// nothing else, so the run may be reordered: the releases more exits make go
/// nearest the return, where the exits that make them can share them. An exit
/// keeps the releases no other exit shares and jumps to the shared tail, so
/// every path gives back exactly what it gave back before. Runs of fewer than
/// two releases stay as they are, since one costs no more than the jump.
pub fn share_exit_tails(body: &mut Body) {
    let mentions = local_mentions(body);
    let exits = take_exit_runs(body, &mentions);
    let (nodes, paths) = tail_trie(&exits);
    let (cuts, shared) = shared_tails(&nodes, &paths);

    // A shared tail runs its own releases and then the tail of the nearest
    // shared node beneath it, or returns. Its releases answer into units of
    // their own, so no unit is written in two places.
    let first_new = body.blocks.len();
    let block_of: HashMap<usize, usize> = shared
        .iter()
        .enumerate()
        .map(|(i, &node)| (node, first_new + i))
        .collect();
    let target = |block: usize| BlockId(u32::try_from(block).expect("block overflow"));
    let mut new_blocks = Vec::with_capacity(shared.len());
    for &node in &shared {
        let mut stmts = Vec::new();
        let mut at = Some(node);
        while let Some(current) = at {
            if current != node && block_of.contains_key(&current) {
                break;
            }
            let (ei, ri) = nodes[current].stmt;
            let mut stmt = exits[ei].run[ri].1.clone();
            if let StatementKind::Assign { place, .. } = &mut stmt.kind {
                let decl = body.locals[place.local.0 as usize].clone();
                place.local = Local(u32::try_from(body.locals.len()).expect("local overflow"));
                body.locals.push(decl);
            }
            stmts.push(stmt);
            at = nodes[current].parent;
        }
        let rep = exits[nodes[node].stmt.0].block;
        new_blocks.push(BasicBlock {
            id: target(block_of[&node]),
            stmts,
            terminator: at.map_or(Terminator::Return, |below| Terminator::Goto {
                target: target(block_of[&below]),
            }),
            span: body.blocks[rep].span,
            terminator_span: body.blocks[rep].terminator_span,
            terminator_inlined: body.blocks[rep].terminator_inlined.clone(),
        });
    }

    for (exit, cut) in exits.into_iter().zip(cuts) {
        let block = &mut body.blocks[exit.block];
        let Some(cut) = cut else {
            block.stmts.extend(exit.run.into_iter().map(|(_, stmt)| stmt));
            continue;
        };
        let kept = exit.order.len() - nodes[cut].depth;
        let mut run: Vec<Option<Statement>> =
            exit.run.into_iter().map(|(_, stmt)| Some(stmt)).collect();
        block
            .stmts
            .extend(exit.order[..kept].iter().filter_map(|&ri| run[ri].take()));
        block.terminator = Terminator::Goto {
            target: target(block_of[&cut]),
        };
    }
    body.blocks.extend(new_blocks);
}

/// How many statements and terminators of `body` mention each local.
fn local_mentions(body: &Body) -> Vec<u32> {
    let mut mentions = vec![0_u32; body.locals.len()];
    for block in &body.blocks {
        let terminator = term_mentioned_locals(&block.terminator);
        for local in block
            .stmts
            .iter()
            .flat_map(stmt_mentioned_locals)
            .chain(terminator)
        {
            if let Some(count) = mentions.get_mut(local.0 as usize) {
                *count += 1;
            }
        }
    }

    mentions
}

/// Splits off the run of releases each returning block ends with, for blocks
/// with at least two, each with the order the shared tails use.
fn take_exit_runs(body: &mut Body, mentions: &[u32]) -> Vec<ExitRun> {
    let mut exits: Vec<ExitRun> = Vec::new();
    for (bi, block) in body.blocks.iter_mut().enumerate() {
        if !matches!(block.terminator, Terminator::Return) {
            continue;
        }
        let start = block
            .stmts
            .iter()
            .rposition(|stmt| {
                !matches!(stmt.kind, StatementKind::Nop)
                    && exit_release_key(stmt, mentions).is_none()
            })
            .map_or(0, |last_other| last_other + 1);
        let releases = block.stmts[start..]
            .iter()
            .filter(|stmt| !matches!(stmt.kind, StatementKind::Nop))
            .count();
        if releases < 2 {
            continue;
        }
        let run = block
            .stmts
            .drain(start..)
            .filter(|stmt| !matches!(stmt.kind, StatementKind::Nop))
            .filter_map(|stmt| Some((exit_release_key(&stmt, mentions)?, stmt)))
            .collect();
        exits.push(ExitRun {
            block: bi,
            run,
            order: Vec::new(),
        });
    }

    let mut makers: HashMap<&str, usize> = HashMap::new();
    for exit in &exits {
        let mut keys: Vec<&str> = exit.run.iter().map(|(key, _)| key.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            *makers.entry(key).or_default() += 1;
        }
    }
    let orders: Vec<Vec<usize>> = exits
        .iter()
        .map(|exit| {
            let mut order: Vec<usize> = (0..exit.run.len()).collect();
            order.sort_by_key(|&i| makers[exit.run[i].0.as_str()]);
            order
        })
        .collect();
    for (exit, order) in exits.iter_mut().zip(orders) {
        exit.order = order;
    }

    exits
}

/// The trie of the exits' runs read from the return backwards, and the path
/// each exit takes through it.
fn tail_trie(exits: &[ExitRun]) -> (Vec<TailNode>, Vec<Vec<usize>>) {
    let mut nodes: Vec<TailNode> = Vec::new();
    let mut child: HashMap<(Option<usize>, &str), usize> = HashMap::new();
    let mut paths: Vec<Vec<usize>> = Vec::with_capacity(exits.len());
    for (ei, exit) in exits.iter().enumerate() {
        let mut parent = None;
        let mut path = Vec::with_capacity(exit.run.len());
        for &ri in exit.order.iter().rev() {
            let key = exit.run[ri].0.as_str();
            let node = *child.entry((parent, key)).or_insert_with(|| {
                nodes.push(TailNode {
                    parent,
                    depth: path.len() + 1,
                    stmt: (ei, ri),
                    exits: 0,
                });
                nodes.len() - 1
            });
            nodes[node].exits += 1;
            path.push(node);
            parent = Some(node);
        }
        paths.push(path);
    }

    (nodes, paths)
}

/// The node each exit is cut at, if any, and every node that gets a shared
/// block of its own.
fn shared_tails(nodes: &[TailNode], paths: &[Vec<usize>]) -> (Vec<Option<usize>>, Vec<usize>) {
    // Each exit is cut at the deepest node another exit shares with it.
    let cuts: Vec<Option<usize>> = paths
        .iter()
        .map(|path| {
            path.iter()
                .copied()
                .take_while(|&node| nodes[node].exits >= 2)
                .last()
                .filter(|&node| nodes[node].depth >= 2)
        })
        .collect();
    // A node gets a block of its own when an exit is cut there, or when two
    // of its branches lead to such blocks, so no release is copied twice. A
    // parent is created before its children, so walking the nodes backwards
    // visits every child first.
    let mut shared = vec![false; nodes.len()];
    for &cut in cuts.iter().flatten() {
        shared[cut] = true;
    }
    let mut branches_below = vec![0_usize; nodes.len()];
    for node in (0..nodes.len()).rev() {
        if !shared[node]
            && branches_below[node] >= 2
            && nodes[node].exits >= 2
            && nodes[node].depth >= 2
        {
            shared[node] = true;
        }
        if let Some(parent) = nodes[node].parent
            && (shared[node] || branches_below[node] > 0)
        {
            branches_below[parent] += 1;
        }
    }
    let shared = (0..nodes.len()).filter(|&node| shared[node]).collect();

    (cuts, shared)
}
