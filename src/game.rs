use crate::bet_size::*;
use crate::interface::*;
use crate::mutex_like::*;
use crate::range::*;
use holdem_hand_evaluator::Hand;
use ndarray::prelude::*;
use rayon::prelude::*;
use std::mem::{size_of, swap};
use std::sync::atomic::{AtomicU64, Ordering};

/// A struct representing a post-flop game.
#[derive(Default)]
pub struct PostFlopGame {
    /// Post-flop game configuration.
    config: GameConfig,

    // computed from `config`
    root: MutexLike<PostFlopNode>,
    num_combinations_inv: f64,
    initial_reach: [Array1<f32>; 2],
    private_hand_cards: [Vec<(u8, u8)>; 2],
    same_hand_index: [Vec<Option<usize>>; 2],
    hand_strength: Vec<[HandStrength; 2]>,
}

/// A struct representing a node in post-flop game tree.
pub struct PostFlopNode {
    player: u16,
    turn: u8,
    river: u8,
    amount: i32,
    children: Vec<(Action, MutexLike<PostFlopNode>)>,
    iso_chances: Vec<IsomorphicChance>,
    cum_regret: Array2<f32>,
    strategy: Array2<f32>,
}

/// A struct for post-flop game configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct GameConfig {
    pub flop: [u8; 3],
    pub initial_pot: i32,
    pub initial_stack: i32,
    pub range: [Range; 2],
    pub flop_bet_sizes: [BetSizeCandidates; 2],
    pub turn_bet_sizes: [BetSizeCandidates; 2],
    pub river_bet_sizes: [BetSizeCandidates; 2],
    pub max_num_bet: i32,
}

/// Possible actions in a post-flop game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    None,
    Fold,
    Check,
    Call,
    Bet(i32),
    Raise(i32),
    AllIn,
    Chance(u8),
}

#[derive(Debug, Clone, Default)]
struct HandStrength {
    opponent_increasing_index: Vec<usize>,
    exclude_threshold: usize,
    win_threshold: Vec<usize>,
    tie_threshold: Vec<usize>,
}

#[derive(Debug, Clone)]
struct BuildTreeInfo<'a> {
    last_action: Action,
    last_bet: [i32; 2],
    num_bet: i32,
    allin_flag: bool,
    current_memory_usage: &'a AtomicU64,
    additional_memory_usage: &'a AtomicU64,
}

/// The index of player who is out of position.
#[allow(dead_code)]
const PLAYER_OOP: u16 = 0;

/// The index of player who is in position.
#[allow(dead_code)]
const PLAYER_IP: u16 = 1;

const PLAYER_CHANCE: u16 = 0xff;
const PLAYER_MASK: u16 = 0xff;
const PLAYER_TERMINAL_FLAG: u16 = 0x100;
const PLAYER_FOLD_FLAG: u16 = 0x300;

const NOT_DEALT: u8 = 0xff;

impl Game for PostFlopGame {
    type Node = PostFlopNode;

    #[inline]
    fn root(&self) -> MutexGuardLike<Self::Node> {
        self.root.lock()
    }

    #[inline]
    fn num_private_hands(&self, player: usize) -> usize {
        self.private_hand_cards[player].len()
    }

    #[inline]
    fn initial_reach(&self, player: usize) -> &Array1<f32> {
        &self.initial_reach[player]
    }

    fn evaluate(
        &self,
        result: &mut ArrayViewMut1<f32>,
        node: &Self::Node,
        player: usize,
        cfreach: &ArrayView1<f32>,
    ) {
        let board_mask: u64 = (1 << self.config.flop[0])
            | (1 << self.config.flop[1])
            | (1 << self.config.flop[2])
            | (1 << node.turn)
            | (1 << node.river);

        // someone folded
        if node.player & PLAYER_FOLD_FLAG == PLAYER_FOLD_FLAG {
            let folded_player = node.player & PLAYER_MASK;
            let payoff = if folded_player as usize == player {
                -node.amount
            } else {
                self.config.initial_pot + node.amount
            };
            let payoff_normalized = payoff as f64 * self.num_combinations_inv;

            let mut cfreach_sum = 0.0;
            let mut cfreach_minus = [0.0; 52];
            for (i, &(c1, c2)) in self.private_hand_cards[player ^ 1].iter().enumerate() {
                let hand_mask: u64 = (1 << c1) | (1 << c2);
                if hand_mask & board_mask == 0 {
                    cfreach_sum += cfreach[i] as f64;
                    cfreach_minus[c1 as usize] += cfreach[i] as f64;
                    cfreach_minus[c2 as usize] += cfreach[i] as f64;
                }
            }

            let player_cards = &self.private_hand_cards[player];
            let same_hand_index = &self.same_hand_index[player];

            result.iter_mut().enumerate().for_each(|(i, cfv)| {
                let (c1, c2) = player_cards[i];
                let hand_mask: u64 = (1 << c1) | (1 << c2);
                if hand_mask & board_mask == 0 {
                    // inclusion-exclusion principle
                    let cfreach = cfreach_sum
                        - (cfreach_minus[c1 as usize] + cfreach_minus[c2 as usize])
                        + same_hand_index[i].map_or(0.0, |j| cfreach[j] as f64);
                    *cfv = (payoff_normalized * cfreach) as f32;
                }
            });
        }
        // showdown
        else {
            let hand_strength = &self.hand_strength[board_index(node.turn, node.river)][player];
            let mut cfreach_sum: Vec<f64> = Vec::with_capacity(cfreach.len() + 1);
            let mut cfreach_minus: Vec<[f64; 52]> = Vec::with_capacity(cfreach.len() + 1);
            cfreach_sum.push(0.0);
            cfreach_minus.push([0.0; 52]);

            let opponent_cards = &self.private_hand_cards[player ^ 1];
            hand_strength
                .opponent_increasing_index
                .iter()
                .for_each(|&index| {
                    cfreach_sum.push(*cfreach_sum.last().unwrap() + cfreach[index] as f64);
                    cfreach_minus.extend_from_within(cfreach_minus.len() - 1..);
                    let (c1, c2) = opponent_cards[index];
                    let last = cfreach_minus.last_mut().unwrap();
                    last[c1 as usize] += cfreach[index] as f64;
                    last[c2 as usize] += cfreach[index] as f64;
                });

            let win_payoff = (self.config.initial_pot + node.amount) as f64;
            let tie_payoff = self.config.initial_pot as f64 * 0.5;
            let lose_payoff = -node.amount as f64;

            let player_cards = &self.private_hand_cards[player];
            let same_hand_index = &self.same_hand_index[player];

            let exclude_threshold = hand_strength.exclude_threshold;
            let lose_threshold = cfreach.len();

            result.iter_mut().enumerate().for_each(|(i, cfv)| {
                let (c1, c2) = player_cards[i];
                let (c1, c2) = (c1 as usize, c2 as usize);
                let hand_mask: u64 = (1 << c1) | (1 << c2);

                if hand_mask & board_mask == 0 {
                    let win_threshold = hand_strength.win_threshold[i];
                    let tie_threshold = hand_strength.tie_threshold[i];

                    let win_cfreach = (cfreach_sum[win_threshold] - cfreach_sum[exclude_threshold])
                        - (cfreach_minus[win_threshold][c1] - cfreach_minus[exclude_threshold][c1])
                        - (cfreach_minus[win_threshold][c2] - cfreach_minus[exclude_threshold][c2]);
                    let tie_cfreach = (cfreach_sum[tie_threshold] - cfreach_sum[win_threshold])
                        - (cfreach_minus[tie_threshold][c1] - cfreach_minus[win_threshold][c1])
                        - (cfreach_minus[tie_threshold][c2] - cfreach_minus[win_threshold][c2])
                        + same_hand_index[i].map_or(0.0, |j| cfreach[j] as f64);
                    let lose_cfreach = (cfreach_sum[lose_threshold] - cfreach_sum[tie_threshold])
                        - (cfreach_minus[lose_threshold][c1] - cfreach_minus[tie_threshold][c1])
                        - (cfreach_minus[lose_threshold][c2] - cfreach_minus[tie_threshold][c2]);

                    *cfv = ((win_payoff * win_cfreach
                        + tie_payoff * tie_cfreach
                        + lose_payoff * lose_cfreach)
                        * self.num_combinations_inv) as f32;
                }
            });
        }
    }
}

impl PostFlopGame {
    /// Constructs a new `PostFlopGame` instance with the given configuration.
    pub fn new(config: &GameConfig, max_memory_mb: Option<u32>) -> Result<Self, String> {
        let mut game = Self::default();
        game.update_config(config, max_memory_mb)?;
        Ok(game)
    }

    /// Updates the game configuration.
    pub fn update_config(
        &mut self,
        config: &GameConfig,
        max_memory_mb: Option<u32>,
    ) -> Result<(), String> {
        self.config = config.clone();
        self.check_config()?;
        self.init(max_memory_mb)?;
        Ok(())
    }

    /// Checks the configuration for errors.
    fn check_config(&mut self) -> Result<(), String> {
        let flop = self.config.flop;

        if flop.iter().any(|&c| c == NOT_DEALT) {
            return Err("Flop cards not initialized".to_string());
        }

        if flop.iter().any(|&c| 52 <= c) {
            return Err(format!("Flop cards must be in [0, 52): flop = {:?}", flop));
        }

        if flop[0] == flop[1] || flop[0] == flop[2] || flop[1] == flop[2] {
            return Err(format!("Flop cards must be unique: flop = {:?}", flop));
        }

        if self.config.initial_pot <= 0 {
            return Err(format!(
                "Initial pot must be positive: initial_pot = {}",
                self.config.initial_pot
            ));
        }

        if self.config.initial_stack <= 0 {
            return Err(format!(
                "Initial stack must be positive: initial_stack = {}",
                self.config.initial_stack
            ));
        }

        if self.config.range[PLAYER_OOP as usize].is_empty() {
            return Err("OOP's range not initialized".to_string());
        }

        if self.config.range[PLAYER_IP as usize].is_empty() {
            return Err("IP's range not initialized".to_string());
        }

        let mut num_combinations = 0.0;
        let flop_mask: u64 = (1 << flop[0]) | (1 << flop[1]) | (1 << flop[2]);
        for c1 in 0..52 {
            for c2 in c1 + 1..52 {
                let oop_mask: u64 = (1 << c1) | (1 << c2);
                let oop_prob = self.config.range[PLAYER_OOP as usize].get_data(c1, c2);
                if oop_mask & flop_mask == 0 && oop_prob > 0.0 {
                    for c3 in 0..52 {
                        for c4 in c3 + 1..52 {
                            let ip_mask: u64 = (1 << c3) | (1 << c4);
                            let ip_prob = self.config.range[PLAYER_IP as usize].get_data(c3, c4);
                            if ip_mask & (flop_mask | oop_mask) == 0 {
                                num_combinations += oop_prob as f64 * ip_prob as f64;
                            }
                        }
                    }
                }
            }
        }

        if num_combinations == 0.0 {
            return Err("Valid card assignment does not exist".to_string());
        }

        self.num_combinations_inv = 1.0 / num_combinations;

        Ok(())
    }

    /// Initializes the game.
    fn init(&mut self, max_memory_mb: Option<u32>) -> Result<(), String> {
        self.init_range();
        self.init_hand_strength();
        self.init_root(max_memory_mb)?;
        Ok(())
    }

    /// Initializes fields `initial_reach`, `private_hand_cards` and `same_hand_index`.
    fn init_range(&mut self) {
        let mut initial_reach_0 = Vec::new();
        let mut initial_reach_1 = Vec::new();

        for player in 0..2 {
            let range = &self.config.range[player];
            let initial_reach = if player == 0 {
                &mut initial_reach_0
            } else {
                &mut initial_reach_1
            };
            let private_hand_cards = &mut self.private_hand_cards[player];
            private_hand_cards.clear();

            for card1 in 0..52 {
                for card2 in card1 + 1..52 {
                    let prob = range.get_data(card1, card2);
                    if prob > 0.0 {
                        initial_reach.push(prob);
                        private_hand_cards.push((card1, card2));
                    }
                }
            }
        }

        self.initial_reach = [
            Array1::from_vec(initial_reach_0),
            Array1::from_vec(initial_reach_1),
        ];

        for player in 0..2 {
            let same_hand_index = &mut self.same_hand_index[player];
            same_hand_index.clear();

            let player_hands = &self.private_hand_cards[player];
            let opponent_hands = &self.private_hand_cards[player ^ 1];
            for hand in player_hands {
                same_hand_index.push(opponent_hands.binary_search(hand).ok());
            }
        }
    }

    /// Initializes a field `hand_strength`.
    fn init_hand_strength(&mut self) {
        let mut flop = Hand::new();
        for card in &self.config.flop {
            flop = flop.add_card(*card as usize);
        }

        let private_hand_cards = &self.private_hand_cards;

        self.hand_strength = (0..52)
            .into_par_iter()
            .flat_map(|board1| {
                (board1 + 1..52).into_par_iter().map(move |board2| {
                    if !flop.contains(board1 as usize) && !flop.contains(board2 as usize) {
                        let board = flop.add_card(board1 as usize).add_card(board2 as usize);
                        let mut strength = [Vec::new(), Vec::new()];
                        let mut strength_sorted = [Vec::new(), Vec::new()];

                        for player in 0..2 {
                            strength[player] = private_hand_cards[player]
                                .iter()
                                .map(|&(hand1, hand2)| {
                                    let (hand1, hand2) = (hand1 as usize, hand2 as usize);
                                    if board.contains(hand1) || board.contains(hand2) {
                                        0
                                    } else {
                                        board.add_card(hand1).add_card(hand2).evaluate()
                                    }
                                })
                                .collect();

                            strength_sorted[player] = strength[player]
                                .iter()
                                .enumerate()
                                .map(|(i, &val)| (val, i))
                                .collect();
                            strength_sorted[player].sort_unstable();
                        }

                        let mut hand_strength: [HandStrength; 2] = Default::default();
                        for player in 0..2 {
                            let sorted = &strength_sorted[player ^ 1];
                            let opponent_increasing_index =
                                sorted.iter().map(|&(_, i)| i).collect();
                            let exclude_threshold =
                                sorted.partition_point(|&(opp_val, _)| opp_val == 0);
                            let win_threshold = strength[player]
                                .iter()
                                .map(|&val| sorted.partition_point(|&(opp_val, _)| opp_val < val))
                                .collect();
                            let tie_threshold = strength[player]
                                .iter()
                                .map(|&val| sorted.partition_point(|&(opp_val, _)| opp_val <= val))
                                .collect();
                            hand_strength[player] = HandStrength {
                                opponent_increasing_index,
                                exclude_threshold,
                                win_threshold,
                                tie_threshold,
                            };
                        }

                        hand_strength
                    } else {
                        Default::default()
                    }
                })
            })
            .collect();
    }

    /// Initializes the root node of game tree.
    fn init_root(&self, max_memory_mb: Option<u32>) -> Result<(), String> {
        let current_memory_usage = AtomicU64::new(size_of::<PostFlopNode>() as u64);
        let additional_memory_usage = AtomicU64::new(0);

        let info = BuildTreeInfo {
            last_action: Action::None,
            last_bet: [0, 0],
            num_bet: 0,
            allin_flag: false,
            current_memory_usage: &current_memory_usage,
            additional_memory_usage: &additional_memory_usage,
        };

        self.root().children.clear();
        self.build_tree_recursive(&mut self.root(), &info);

        // memory usage check
        let total_memory_usage = current_memory_usage.load(Ordering::Relaxed)
            + additional_memory_usage.load(Ordering::Relaxed);
        if max_memory_mb.is_some()
            && total_memory_usage > max_memory_mb.unwrap() as u64 * 1024 * 1024
        {
            return Err(format!(
                "Memory usage {} MB exceeds the limit {} MB",
                total_memory_usage / 1024 / 1024,
                max_memory_mb.unwrap()
            ));
        }

        self.allocate_memory_recursive(&mut self.root());

        Ok(())
    }

    /// Builds the game tree recursively.
    fn build_tree_recursive(&self, node: &mut PostFlopNode, info: &BuildTreeInfo) {
        if node.is_terminal() {
            return;
        }

        // chance node
        if node.is_chance() {
            self.push_chances(node, info);

            node.children.par_iter().for_each(|(last_action, child)| {
                self.build_tree_recursive(
                    &mut child.lock(),
                    &BuildTreeInfo {
                        last_action: *last_action,
                        last_bet: [0, 0],
                        num_bet: 0,
                        ..*info
                    },
                );
            });
        }
        // player node
        else {
            self.push_actions(node, info);

            node.children.par_iter().for_each(|(action, child)| {
                let mut last_bet = info.last_bet;
                let mut num_bet = info.num_bet;
                let mut allin_flag = info.allin_flag;

                match *action {
                    Action::Call => {
                        last_bet[node.player as usize] = last_bet[node.player as usize ^ 1];
                    }
                    Action::Bet(size) | Action::Raise(size) => {
                        last_bet[node.player as usize] = size;
                        num_bet += 1;
                    }
                    Action::AllIn => {
                        last_bet[node.player as usize] += self.config.initial_stack - node.amount;
                        num_bet += 1;
                        allin_flag = true;
                    }
                    _ => {}
                }

                self.build_tree_recursive(
                    &mut child.lock(),
                    &BuildTreeInfo {
                        last_action: *action,
                        last_bet,
                        num_bet,
                        allin_flag,
                        ..*info
                    },
                )
            });
        }
    }

    /// Pushes the chance actions to the `node`.
    fn push_chances(&self, node: &mut PostFlopNode, info: &BuildTreeInfo) {
        let flop = self.config.flop;
        let flop_mask: u64 = (1 << flop[0]) | (1 << flop[1]) | (1 << flop[2]);

        let mut indices = [0; 52];

        let mut flop_rankset = [0; 4];
        for card in flop {
            let rank = card >> 2;
            let suit = card & 3;
            flop_rankset[suit as usize] |= 1 << rank;
        }

        // deal turn
        if node.turn == NOT_DEALT {
            let next_player = if !info.allin_flag {
                PLAYER_OOP
            } else {
                PLAYER_CHANCE
            };

            let mut iso_suits = [None; 4];
            for suit1 in 0..4 {
                for suit2 in suit1 + 1..4 {
                    if iso_suits[suit2 as usize].is_none()
                        && flop_rankset[suit1 as usize] == flop_rankset[suit2 as usize]
                    {
                        iso_suits[suit2 as usize] = Some(suit1);
                    }
                }
            }

            for card in 0..52 {
                if (1 << card) & flop_mask != 0 {
                    continue;
                }

                let rank = card >> 2;
                let suit = card & 3;

                // isomorphic chance
                if let Some(iso_suit) = iso_suits[suit as usize] {
                    let iso_card = rank << 2 | iso_suit;
                    let iso_index = indices[iso_card as usize];
                    let mut iso_chance = IsomorphicChance {
                        index: iso_index,
                        swap_list: Default::default(),
                    };

                    for player in 0..2 {
                        let cards = &self.private_hand_cards[player];
                        for i in 0..cards.len() {
                            let (c1, c2) = cards[i];
                            if c1 == card {
                                if let Ok(j) = cards.binary_search(&(iso_card, c2)) {
                                    iso_chance.swap_list[player].push((i, j));
                                }
                            }
                            if c2 == card {
                                if let Ok(j) = cards.binary_search(&(c1, iso_card)) {
                                    iso_chance.swap_list[player].push((i, j));
                                }
                            }
                        }
                        iso_chance.swap_list[player].shrink_to_fit();
                        info.current_memory_usage.fetch_add(
                            vec_memory_usage(&iso_chance.swap_list[player]),
                            Ordering::Relaxed,
                        );
                    }

                    node.iso_chances.push(iso_chance);
                }
                // normal chance
                else {
                    indices[card as usize] = node.children.len();
                    node.children.push((
                        Action::Chance(card),
                        MutexLike::new(PostFlopNode {
                            player: next_player,
                            turn: card,
                            amount: node.amount,
                            ..Default::default()
                        }),
                    ));
                }
            }
        }
        // deal river
        else {
            let turn_mask = flop_mask | (1 << node.turn);
            let mut turn_rankset = flop_rankset;
            turn_rankset[node.turn as usize & 3] |= 1 << (node.turn >> 2);

            let next_player = if !info.allin_flag {
                PLAYER_OOP
            } else {
                PLAYER_TERMINAL_FLAG
            };

            let mut iso_suits = [None; 4];
            for suit1 in 0..4 {
                for suit2 in suit1 + 1..4 {
                    if iso_suits[suit2 as usize].is_none()
                        && flop_rankset[suit1 as usize] == flop_rankset[suit2 as usize]
                        && turn_rankset[suit1 as usize] == turn_rankset[suit2 as usize]
                    {
                        iso_suits[suit2 as usize] = Some(suit1);
                    }
                }
            }

            for card in 0..52 {
                if (1 << card) & turn_mask != 0 {
                    continue;
                }

                let rank = card >> 2;
                let suit = card & 3;

                // isomorphic chance
                if let Some(iso_suit) = iso_suits[suit as usize] {
                    let iso_card = rank << 2 | iso_suit;
                    let iso_index = indices[iso_card as usize];
                    let mut iso_chance = IsomorphicChance {
                        index: iso_index,
                        swap_list: Default::default(),
                    };

                    for player in 0..2 {
                        let cards = &self.private_hand_cards[player];
                        for i in 0..cards.len() {
                            let (c1, c2) = cards[i];
                            if c1 == card {
                                if let Ok(j) = cards.binary_search(&(iso_card, c2)) {
                                    iso_chance.swap_list[player].push((i, j));
                                }
                            }
                            if c2 == card {
                                if let Ok(j) = cards.binary_search(&(c1, iso_card)) {
                                    iso_chance.swap_list[player].push((i, j));
                                }
                            }
                        }
                        iso_chance.swap_list[player].shrink_to_fit();
                        info.current_memory_usage.fetch_add(
                            vec_memory_usage(&iso_chance.swap_list[player]),
                            Ordering::Relaxed,
                        );
                    }

                    node.iso_chances.push(iso_chance);
                }
                // normal chance
                else {
                    indices[card as usize] = node.children.len();
                    node.children.push((
                        Action::Chance(card),
                        MutexLike::new(PostFlopNode {
                            player: next_player,
                            turn: node.turn,
                            river: card,
                            amount: node.amount,
                            ..Default::default()
                        }),
                    ));
                }
            }
        }

        node.children.shrink_to_fit();
        node.iso_chances.shrink_to_fit();

        info.current_memory_usage.fetch_add(
            vec_memory_usage(&node.children) + vec_memory_usage(&node.iso_chances),
            Ordering::Relaxed,
        );
    }

    /// Pushes the actions to the `node`.
    fn push_actions(&self, node: &mut PostFlopNode, info: &BuildTreeInfo) {
        let player = node.player;
        let player_opponent = node.player ^ 1;

        let player_bet = info.last_bet[player as usize];
        let opponent_bet = info.last_bet[player_opponent as usize];

        let bet_diff = opponent_bet - player_bet;
        let pot = self.config.initial_pot + 2 * (node.amount + bet_diff);

        let (candidates, is_river) = if node.turn == NOT_DEALT {
            (&self.config.flop_bet_sizes, false)
        } else if node.river == NOT_DEALT {
            (&self.config.turn_bet_sizes, false)
        } else {
            (&self.config.river_bet_sizes, true)
        };

        let player_after_call = if is_river {
            PLAYER_TERMINAL_FLAG
        } else {
            PLAYER_CHANCE
        };

        let player_after_check = if player == PLAYER_OOP {
            player_opponent
        } else {
            player_after_call
        };

        let mut actions = Vec::new();

        if matches!(
            info.last_action,
            Action::None | Action::Check | Action::Chance(_)
        ) {
            // add check
            actions.push((Action::Check, player_after_check));

            // add first bet
            if info.num_bet < self.config.max_num_bet {
                for &bet_size in &candidates[player as usize].bet {
                    match bet_size {
                        BetSize::PotRelative(ratio) => {
                            let size = (pot as f32 * ratio).round() as i32;
                            actions.push((Action::Bet(size), player_opponent));
                        }
                        BetSize::LastBetRelative(_) => panic!("unexpected bet size"),
                    }
                }
            }
        } else {
            // add fold
            actions.push((Action::Fold, PLAYER_FOLD_FLAG | player));

            // add call
            actions.push((Action::Call, player_after_call));

            // add raise
            if !info.allin_flag && info.num_bet < self.config.max_num_bet {
                for &bet_size in &candidates[player as usize].raise {
                    match bet_size {
                        BetSize::PotRelative(ratio) => {
                            let size = opponent_bet + (pot as f32 * ratio).round() as i32;
                            actions.push((Action::Raise(size), player_opponent));
                        }
                        BetSize::LastBetRelative(ratio) => {
                            let size = (opponent_bet as f32 * ratio).round() as i32;
                            actions.push((Action::Raise(size), player_opponent));
                        }
                    }
                }
            }
        }

        let max_bet = self.config.initial_stack - node.amount + player_bet;
        let min_bet = max_bet.min(opponent_bet + bet_diff);

        // adjust bet sizes
        for (action, _) in actions.iter_mut() {
            match *action {
                Action::Bet(size) => {
                    let adjusted_size = size.clamp(min_bet, max_bet);
                    if adjusted_size == max_bet {
                        *action = Action::AllIn;
                    } else if size != adjusted_size {
                        *action = Action::Bet(adjusted_size);
                    }
                }
                Action::Raise(size) => {
                    let adjusted_size = size.clamp(min_bet, max_bet);
                    if adjusted_size == max_bet {
                        *action = Action::AllIn;
                    } else if size != adjusted_size {
                        *action = Action::Raise(adjusted_size);
                    }
                }
                _ => {}
            }
        }

        // remove duplicates
        actions.sort_unstable();
        actions.dedup();

        // push actions
        for (action, next_player) in actions {
            let mut amount = node.amount;
            if matches!(
                action,
                Action::Call | Action::Bet(_) | Action::Raise(_) | Action::AllIn
            ) {
                amount += bet_diff;
            }

            node.children.push((
                action,
                MutexLike::new(PostFlopNode {
                    player: next_player,
                    turn: node.turn,
                    river: node.river,
                    amount,
                    ..Default::default()
                }),
            ));
        }

        node.children.shrink_to_fit();
        info.current_memory_usage
            .fetch_add(vec_memory_usage(&node.children), Ordering::Relaxed);

        let num_elems = node.num_actions() * self.num_private_hands(player as usize);
        info.additional_memory_usage
            .fetch_add((2 * num_elems * size_of::<f32>()) as u64, Ordering::Relaxed);
    }

    /// Allocates memory recursively.
    fn allocate_memory_recursive(&self, node: &mut PostFlopNode) {
        if node.is_terminal() {
            return;
        }

        if !node.is_chance() {
            let num_actions = node.num_actions();
            let num_private_hands = self.num_private_hands(node.player());
            node.cum_regret = Array2::zeros((num_actions, num_private_hands));
            node.strategy = Array2::zeros((num_actions, num_private_hands));
        }

        node.actions().into_par_iter().for_each(|action| {
            self.allocate_memory_recursive(&mut node.play(action));
        });
    }
}

impl GameNode for PostFlopNode {
    #[inline]
    fn is_terminal(&self) -> bool {
        self.player & PLAYER_TERMINAL_FLAG != 0
    }

    #[inline]
    fn is_chance(&self) -> bool {
        self.player == PLAYER_CHANCE
    }

    #[inline]
    fn player(&self) -> usize {
        self.player as usize
    }

    #[inline]
    fn num_actions(&self) -> usize {
        self.children.len()
    }

    #[inline]
    fn chance_factor(&self) -> f32 {
        [1.0 / 45.0, 1.0 / 44.0][(self.turn != NOT_DEALT) as usize]
    }

    #[inline]
    fn isomorphic_chances(&self) -> &Vec<IsomorphicChance> {
        &self.iso_chances
    }

    #[inline]
    fn play(&self, action: usize) -> MutexGuardLike<Self> {
        self.children[action].1.lock()
    }

    #[inline]
    fn cum_regret(&self) -> &Array2<f32> {
        &self.cum_regret
    }

    #[inline]
    fn cum_regret_mut(&mut self) -> &mut Array2<f32> {
        &mut self.cum_regret
    }

    #[inline]
    fn strategy(&self) -> &Array2<f32> {
        &self.strategy
    }

    #[inline]
    fn strategy_mut(&mut self) -> &mut Array2<f32> {
        &mut self.strategy
    }
}

impl Default for PostFlopNode {
    fn default() -> Self {
        Self {
            player: PLAYER_OOP,
            turn: NOT_DEALT,
            river: NOT_DEALT,
            amount: 0,
            children: Vec::new(),
            iso_chances: Vec::new(),
            cum_regret: Default::default(),
            strategy: Default::default(),
        }
    }
}

impl Default for GameConfig {
    fn default() -> Self {
        Self {
            flop: [NOT_DEALT; 3],
            initial_pot: 0,
            initial_stack: 0,
            range: Default::default(),
            flop_bet_sizes: Default::default(),
            turn_bet_sizes: Default::default(),
            river_bet_sizes: Default::default(),
            max_num_bet: 0,
        }
    }
}

/// Attempts to convert an optionally space-separated string into a sorted flop array.
///
/// Example: `"2c 3d 4h"` -> `Ok([0, 5, 10])`
pub fn flop_from_str(s: &str) -> Result<[u8; 3], String> {
    let mut result = [0; 3];
    let mut chars = s.chars();

    result[0] = card_from_str(&mut chars)?;
    result[1] = card_from_str(&mut chars.by_ref().skip_while(|c| c.is_whitespace()))?;
    result[2] = card_from_str(&mut chars.by_ref().skip_while(|c| c.is_whitespace()))?;

    if chars.next().is_some() {
        return Err("expected three cards".to_string());
    }

    result.sort_unstable();

    if result[0] == result[1] || result[1] == result[2] {
        return Err("cards must be unique".to_string());
    }

    Ok(result)
}

fn card_from_str<T: Iterator<Item = char>>(chars: &mut T) -> Result<u8, String> {
    let rank_char = chars.next().ok_or_else(|| "parse failed".to_string())?;
    let suit_char = chars.next().ok_or_else(|| "parse failed".to_string())?;

    let rank = match rank_char {
        'A' => 12,
        'K' => 11,
        'Q' => 10,
        'J' => 9,
        'T' => 8,
        '2'..='9' => rank_char as u8 - b'2',
        _ => return Err(format!("expected rank: {rank_char}")),
    };

    let suit = match suit_char {
        's' => 3,
        'h' => 2,
        'd' => 1,
        'c' => 0,
        _ => return Err(format!("expected suit: {suit_char}")),
    };

    Ok((rank << 2) | suit)
}

fn vec_memory_usage<T>(vec: &Vec<T>) -> u64 {
    vec.capacity() as u64 * size_of::<T>() as u64
}

fn board_index(mut turn: u8, mut river: u8) -> usize {
    if turn > river {
        swap(&mut turn, &mut river);
    }
    turn as usize * (101 - turn as usize) / 2 + river as usize - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solver::*;
    use crate::utility::*;

    #[test]
    fn test_flop_from_str() {
        let tests = [("Qs Jh 2h", [2, 38, 43]), ("Td9d6h", [18, 29, 33])];
        for test in tests {
            assert_eq!(flop_from_str(test.0).unwrap(), test.1);
        }
    }

    #[test]
    fn all_check_all_range() {
        let all_range_str = "22+,A2+,K2+,Q2+,J2+,T2+,92+,82+,72+,62+,52+,42+,32";
        let config = GameConfig {
            flop: flop_from_str("Td9d6h").unwrap(),
            initial_pot: 80,
            initial_stack: 960,
            range: [all_range_str.parse().unwrap(); 2],
            ..Default::default()
        };
        let game = PostFlopGame::new(&config, None).unwrap();
        normalize_strategy(&game);
        let ev0 = compute_ev(&game, 0);
        let ev1 = compute_ev(&game, 1);
        assert!((ev0 - 40.0).abs() < 1e-5);
        assert!((ev1 - 40.0).abs() < 1e-5);
    }

    #[test]
    fn one_raise_all_range() {
        let all_range_str = "22+,A2+,K2+,Q2+,J2+,T2+,92+,82+,72+,62+,52+,42+,32";
        let config = GameConfig {
            flop: flop_from_str("Td9d6h").unwrap(),
            initial_pot: 80,
            initial_stack: 960,
            range: [all_range_str.parse().unwrap(); 2],
            river_bet_sizes: [bet_sizes_from_str("50%", "").unwrap(), Default::default()],
            max_num_bet: 1,
            ..Default::default()
        };
        let game = PostFlopGame::new(&config, None).unwrap();
        normalize_strategy(&game);
        let ev0 = compute_ev(&game, 0);
        let ev1 = compute_ev(&game, 1);
        assert!((ev0 - 50.0).abs() < 1e-5);
        assert!((ev1 - 30.0).abs() < 1e-5);
    }

    #[test]
    fn always_win() {
        // be careful for straight flushes
        let lose_range_str = "22+,A2+,K9-K2,Q8-Q2,J8-J2,T8-T2,92+,82+,72+,62+";
        let config = GameConfig {
            flop: flop_from_str("AcAdKh").unwrap(),
            initial_pot: 80,
            initial_stack: 960,
            range: ["AA".parse().unwrap(), lose_range_str.parse().unwrap()],
            ..Default::default()
        };
        let game = PostFlopGame::new(&config, None).unwrap();
        normalize_strategy(&game);
        let ev0 = compute_ev(&game, 0);
        let ev1 = compute_ev(&game, 1);
        assert!((ev0 - 80.0).abs() < 1e-5);
        assert!((ev1 - 0.0).abs() < 1e-5);
    }

    #[test]
    fn no_assignment() {
        let config = GameConfig {
            flop: flop_from_str("Td9d6h").unwrap(),
            initial_pot: 80,
            initial_stack: 960,
            range: ["TT".parse().unwrap(), "TT".parse().unwrap()],
            ..Default::default()
        };
        let game = PostFlopGame::new(&config, None);
        assert!(game.is_err());
    }

    #[test]
    #[ignore]
    fn cfr_solve() {
        let bet_sizes = bet_sizes_from_str("50%", "50%").unwrap();
        let config = GameConfig {
            flop: flop_from_str("Td9d6h").unwrap(),
            initial_pot: 80,
            initial_stack: 960,
            range: [
                "22+,A2s+,A8o+,K7s+,K9o+,Q8s+,Q9o+,J8s+,J9o+,T8+,97+,86+,75+,64s+,65o,54,43s"
                    .parse()
                    .unwrap(),
                "22+,A4s+,A9o+,K9s+,KTo+,Q9s+,QTo+,J9+,T9,98s,87s,76s,65s"
                    .parse()
                    .unwrap(),
            ],
            flop_bet_sizes: [bet_sizes.clone(), bet_sizes.clone()],
            turn_bet_sizes: [bet_sizes.clone(), bet_sizes.clone()],
            river_bet_sizes: [bet_sizes.clone(), bet_sizes.clone()],
            max_num_bet: 5,
        };

        let game = PostFlopGame::new(&config, Some(3072)).unwrap();
        solve(&game, 1000, 80.0 * 0.005, 40.0, true);
        let ev0 = compute_ev(&game, 0);
        let ev1 = compute_ev(&game, 1);

        // verified by GTO+
        assert!((ev0 - 35.0).abs() < 0.5);
        assert!((ev1 - 45.0).abs() < 0.5);
    }
}
