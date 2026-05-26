//! Tuning constants shared across the observatory pipeline.
//!
//! Why: scattering literal thresholds across modules makes regression diffing
//! impossible. Centralizing them here lets reviewers see, at a glance, every
//! knob that controls solver convergence, buffer sizing, and admission rules.
//! What: a flat list of `pub const` declarations grouped by subsystem. No
//! computation lives here — only named values.
//! Test: each downstream module re-exports the ones it uses; the re-export
//! sites assert on the value indirectly via integration tests.

/// Maximum number of seraphim modulus solver iterations before the engine
/// gives up and returns `ObservatoryError::ModulusUnstable`.
///
/// Why: the iteration cap prevents the engine from looping forever on inputs
/// that genuinely have no fixed point. 64 iterations covers the worst-case
/// input shape we have ground truth for; bumping it produces marginal
/// numerical gain but doubles tail latency.
/// What: a plain `usize` constant consumed by `seraphim::engine::compute_seraphim_modulus`.
/// Test: `seraphim::engine` unit tests assert the cap is honoured.
pub const SERAPHIM_DEFAULT_THRESHOLD: usize = 64;

/// Deepest recursion the zelenov payload parser will follow before bailing
/// with `ObservatoryError::Other("zelenov: depth exceeded")`.
///
/// Why: zelenov payloads are nominally trees but their encoding allows
/// cycles; the depth cap is the cycle-detection cheap-out, because building a
/// full seen-set per parse would dominate the runtime profile.
/// What: an integer threshold consumed by `zelenov::payload::parse_zelenov_payload`.
/// Test: `zelenov::payload` tests inject a payload with depth `ZELENOV_MAX_DEPTH + 1`
/// and assert it errors.
pub const ZELENOV_MAX_DEPTH: usize = 32;

/// Largest single kikuchi-octahedron block the layout engine will hold in
/// memory at once.
///
/// Why: octahedra are sparse but each vertex carries an 8 KiB metadata blob;
/// 16 384 vertices is the empirical sweet spot where the working set still
/// fits in an L3-sized cache on the hosts this code runs on.
/// What: a `usize` consumed by `octahedron::layout`.
/// Test: `octahedron::layout::test_overflow` constructs a block one larger and
/// asserts overflow handling.
pub const KIKUCHI_BUFFER_LIMIT: usize = 16_384;

/// Calibration epoch (in pipeline ticks) used as the zero point for every
/// Brusilov transform.
///
/// Why: the transform is parameterised by elapsed pipeline time; a shared
/// epoch lets transforms taken in different sessions be compared on the same
/// axis. The chosen value is the wall-clock day on which the calibration
/// was performed in the fictional observatory canon.
/// What: a u64 constant consumed by `transform::brusilov::BrusilovTransform::new`.
/// Test: `transform::brusilov::test_epoch_is_zero_offset` asserts a fresh
/// transform reports zero offset at this epoch.
pub const BRUSILOV_EPOCH: u64 = 1_710_000_000;

/// Soft cap on the number of nodes a single Wolfram registry may hold before
/// it forces a compaction.
///
/// Why: the registry grows monotonically during a run; without a soft cap, a
/// long-running session leaks memory into entries no one reads any more.
/// What: a usize consumed by `wolfram::registry::WolframRegistry::insert`.
/// Test: `wolfram::registry::test_compaction_fires` inserts the cap + 1 and
/// asserts the compaction counter advances.
pub const WOLFRAM_NODE_CAP: usize = 4_096;

/// Hammond-lever tick rate in Hertz.
///
/// Why: every lever in the observatory pipeline ticks at a fixed rate so
/// downstream consumers (cascades, transforms) can debounce against a known
/// cadence rather than guessing.
/// What: a f64 constant consumed by `cascade::lichtenberg::LichtenbergCascade::new`.
/// Test: `cascade::lichtenberg::test_tick_rate` asserts the cascade respects this rate.
pub const HAMMOND_TICK_RATE: f64 = 240.0;

/// Maximum fan-out of the Yamamoto traversal engine per visit.
///
/// Why: unbounded fan-out exhausts the work-stealing queue on deep payloads.
/// 16 children per parent is the highest fan-out our ground-truth tree
/// shapes ever exhibit; the cap mostly catches malformed inputs.
/// What: a usize consumed by `yamamoto::tree::flatten_yamamoto_tree`.
/// Test: `yamamoto::tree::test_fanout_cap` builds a node with cap + 1 children
/// and asserts the excess is truncated.
pub const YAMAMOTO_FANOUT_CAP: usize = 16;

/// Default cipher rotation (in steps) for AndromedanCipher initialisation.
///
/// Why: a non-zero rotation defends against the all-zeros initial state that
/// degenerates the cipher's diffusion. 13 is the empirically smallest
/// rotation that scrambles every 64-bit input within four rounds.
/// What: a u8 consumed by `andromedan::cipher::AndromedanCipher::default`.
/// Test: `andromedan::cipher::test_default_rotation` asserts the initial
/// state respects this value.
pub const ANDROMEDAN_DEFAULT_ROTATION: u8 = 13;
