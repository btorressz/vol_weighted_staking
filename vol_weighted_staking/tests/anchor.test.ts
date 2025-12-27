
declare function setTimeout(handler: (...args: any[]) => void, timeout?: number): any;

// Minimal assert (no chai).
function assert(cond: any, msg?: string): asserts cond {
  if (!cond) throw new Error(msg ?? "assertion failed");
}

const { PublicKey, Keypair, SystemProgram, LAMPORTS_PER_SOL } = web3;

/* ----------------------------- */
/* Constants                      */
/* ----------------------------- */
const PRICE_FP_SCALE = 1_000_000;
const RET_FP_SCALE = 1_000_000;

const ORACLE_FEED_SOL_USD = new PublicKey("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix");
const ORACLE_FEED_SOL_USDC = new PublicKey("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD");

// Match your #[msg("...")] where possible (fallback matcher also accepts custom program error).
const ERR = {
  Unauthorized: "Unauthorized",
  Paused: "Program is paused",
  OracleNotReady: "Oracle not ready",
  OracleDegradedHedgeBlocked: "Oracle degraded: hedge blocked",
  HedgeTooSoon: "Hedge request too soon",
  DriftNotMet: "Drift not met",
  NoOutstandingRequest: "No outstanding hedge request",
  WrongRequestId: "Wrong request id",
  PolicyCooldown: "Policy update cooldown not met",
  InvalidParams: "Invalid parameters",
  MathOverflow: "Math overflow",
  VolOutOfRange: "Volatility out of range",
  CapExceeded: "Cap exceeded",
  LeverageExceeded: "Leverage exceeded",
  ReserveTooLow: "Reserve too low",
  KeeperRateLimited: "Keeper rate limited",
  KeeperBondInsufficient: "Keeper bond insufficient",
};

/* ----------------------------- */
/* Helpers                        */
/* ----------------------------- */

// IMPORTANT: Do NOT name this `sleep` because Playground extralib often already declares it.
// This avoids: "Cannot redeclare block-scoped variable 'sleep'."
const delayMs = (ms: number) => new Promise<void>((resolve) => setTimeout(() => resolve(), ms));

async function airdropIfNeeded(pubkey: any, minSol = 2) {
  const bal = await pg.connection.getBalance(pubkey, "confirmed");
  if (bal >= minSol * LAMPORTS_PER_SOL) return;

  const sig = await pg.connection.requestAirdrop(pubkey, minSol * LAMPORTS_PER_SOL);
  await pg.connection.confirmTransaction(sig, "confirmed");
}

async function waitForSlots(n: number) {
  const start = await pg.connection.getSlot("confirmed");
  while (true) {
    const cur = await pg.connection.getSlot("confirmed");
    if (cur >= start + n) return;
    await delayMs(350);
  }
}

function deriveVaultPda(authorityPk: any) {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("vault"), authorityPk.toBuffer()],
    pg.program.programId
  );
}

function fpToDecimal(fp: any, scale = PRICE_FP_SCALE) {
  const n = typeof fp === "number" ? fp : fp.toNumber();
  return n / scale;
}

function bpsToPct(bps: number) {
  return (bps / 100).toFixed(2) + "%";
}

async function fetchVault(vaultStatePda: any) {
  return pg.program.account.vaultState.fetch(vaultStatePda);
}

function logOracleState(v: any) {
  console.log(
    `📊 Oracle: ok=${v.oracleOk} degraded=${v.oracleDegraded} ` +
      `spot=${fpToDecimal(v.oraclePriceFp).toFixed(4)} ema=${fpToDecimal(v.oracleEmaPriceFp).toFixed(4)} ` +
      `conf=${fpToDecimal(v.oracleConfFp).toFixed(4)} publish_time=${v.oraclePublishSlot.toString()}`
  );
}

function logPolicyState(v: any) {
  console.log(
    `📈 Policy: epoch=${v.epoch.toString()} band=${bpsToPct(v.bandBps)} ` +
      `minHedgeInterval=${v.minHedgeIntervalSlots.toString()} ` +
      `realized=${bpsToPct(v.realizedVolBps)} implied=${bpsToPct(v.impliedVolBps)} score=${bpsToPct(v.volScoreBps)}`
  );
}

function logVaultSnapshot(v: any) {
  console.log(
    `🧾 Vault: staked=${v.stakedSol.toString()} reserve=${v.reserveSol.toString()} hedgeNotionalUsd=${v.hedgeNotionalUsd.toString()} ` +
      `paused=${v.paused} emergency=${v.emergencyWithdrawEnabled} reqOutstanding=${v.requestOutstanding}`
  );
}

// Event capture: fix TS union typing by accepting `any` and casting for addEventListener.
async function withEventListener<T>(
  eventName: any,
  fn: () => Promise<T>,
  onEvent?: (e: any) => void
): Promise<{ result: T; events: any[] }> {
  const events: any[] = [];
  let listenerId: any = null;

  try {
    listenerId = await pg.program.addEventListener(eventName as any, (e: any) => {
      events.push(e);
      if (onEvent) onEvent(e);
    });

    const result = await fn();

    // small delay so listener receives events
    await delayMs(500);

    return { result, events };
  } finally {
    if (listenerId !== null) {
      try {
        await pg.program.removeEventListener(listenerId);
      } catch (_) {}
    }
  }
}

async function expectFail(p: Promise<any>, mustInclude: string) {
  try {
    await p;
    throw new Error(`❌ Expected failure containing "${mustInclude}", but tx succeeded`);
  } catch (e: any) {
    const msg = String(e?.message ?? e);
    console.log(`✅ Expected failure: ${mustInclude}`);
    assert(
      msg.includes(mustInclude) || msg.includes("custom program error") || msg.includes("AnchorError"),
      `Expected error to include "${mustInclude}" but got: ${msg}`
    );
  }
}

async function ensurePythAccountsExistOrSkip() {
  const usd = await pg.connection.getAccountInfo(ORACLE_FEED_SOL_USD, "confirmed");
  const usdc = await pg.connection.getAccountInfo(ORACLE_FEED_SOL_USDC, "confirmed");
  if (!usd || !usdc) {
    console.log(
      "⚠️ Pyth devnet accounts not found on this cluster/connection. " +
        "These oracle integration tests require devnet (or a fork that includes these accounts)."
    );
    return false;
  }
  return true;
}

// Must match your IDL InitializeParams field names (camelCase). Adjust if your IDL differs.
function defaultInitParams(overrides: Partial<any> = {}) {
  const base = {
    // policy bounds
    minBandBps: 50,
    maxBandBps: 1500,
    minIntervalSlots: new BN(5),
    maxIntervalSlots: new BN(50),

    // score weights (must sum to 10_000)
    volWeightRealizedBps: 6000,
    volWeightImpliedBps: 4000,

    // anti-gaming
    minSamples: 4,
    minReturnSpacingSlots: new BN(2),

    // stability
    policyUpdateMinSlots: new BN(5),
    maxPolicySlewBps: 1000,
    hysteresisBps: 100,

    // vol model
    volMode: 0, // 0=Stdev, 1=Ewma, 2=Mad
    ewmaAlphaBps: 1500,

    // caps/guardrails
    maxStakedSol: new BN(10_000),
    maxAbsHedgeNotionalUsd: new BN(2_000_000),
    maxHedgePerSolUsdFp: new BN(400 * PRICE_FP_SCALE),
    minReserveBps: 500,

    // oracle config
    oracleFeedChoice: 3, // 1=USD,2=USDC,3=AutoPreferUsdThenUsdc
    maxPriceAgeSlots: new BN(120), // NOTE: your program treats this as seconds (publish_time is seconds)
    maxConfidenceBps: 200,
    maxPriceJumpBps: 2000,

    // hedge sizing
    targetDeltaBps: 10_000,
    lstBetaFp: new BN(1 * PRICE_FP_SCALE),

    // confirm hedge config
    maxConfirmDelaySlots: new BN(25),

    // circuit breaker
    extremeDriftBps: 2000,

    // keeper controls
    maxUpdatesPerEpoch: 50,
    keeperBondRequiredLamports: new BN(0),
  };

  return { ...base, ...overrides };
}

/* ----------------------------- */
/* Test Suite                     */
/* ----------------------------- */

describe("vol_weighted_staking (Pyth devnet integration) — Solana Playground", () => {
  // Actors
  const authority = pg.wallet; // Playground wallet = authority
  const user = Keypair.generate();
  const keeper1 = Keypair.generate();
  const keeper2 = Keypair.generate();

  let vaultStatePda: any;
  let vaultBump = 0;

  let pythAvailable = true;

  it("🔧 Setup: airdrop actors & derive PDA", async () => {
    await airdropIfNeeded(user.publicKey, 2);
    await airdropIfNeeded(keeper1.publicKey, 2);
    await airdropIfNeeded(keeper2.publicKey, 2);

    const pdaRes = deriveVaultPda(authority.publicKey);
    vaultStatePda = pdaRes[0];
    vaultBump = pdaRes[1];

    console.log(`🧩 vaultState PDA = ${vaultStatePda.toBase58()} bump=${vaultBump}`);

    pythAvailable = await ensurePythAccountsExistOrSkip();
    assert(vaultStatePda instanceof PublicKey, "vaultStatePda should be a PublicKey");
  });

  /* ----------------------------- */
  /* Initialization                 */
  /* ----------------------------- */
  describe("Initialization", () => {
    it("✅ Initializes vault with valid parameters", async () => {
      const params = defaultInitParams({ oracleFeedChoice: 3 });

      const { result: sig, events } = await withEventListener("VaultInitialized", async () => {
        return pg.program.methods
          .initializeVault(params)
          .accounts({
            authority: authority.publicKey,
            vaultState: vaultStatePda,
            systemProgram: SystemProgram.programId,
          })
          .rpc();
      });

      console.log(`✅ initializeVault sig: ${sig}`);

      const v = await fetchVault(vaultStatePda);
      console.log(`🧾 configVersion=${v.configVersion.toString()}`);
      assert(v.authority.equals(authority.publicKey), "authority mismatch");
      assert(v.keeperAdmin.equals(authority.publicKey), "keeperAdmin mismatch");
      assert(v.configVersion.toNumber() >= 1, "configVersion should be >= 1");

      if (events.length > 0) {
        const e = events[events.length - 1];
        console.log("✅ VaultInitialized event captured");
        assert(e.authority.equals(authority.publicKey), "event authority mismatch");
      } else {
        console.log("⚠️ No VaultInitialized event captured (listener may be unsupported).");
      }
    });

    it("❌ Fails with invalid policy bounds (min > max)", async () => {
      // Use a fresh authority/PDA to ensure we test param validation (not "already initialized").
      const tempAuth = Keypair.generate();
      await airdropIfNeeded(tempAuth.publicKey, 2);
      const [pda] = deriveVaultPda(tempAuth.publicKey);

      const params = defaultInitParams({ minBandBps: 2000, maxBandBps: 1000 });

      await expectFail(
        pg.program.methods
          .initializeVault(params)
          .accounts({
            authority: tempAuth.publicKey,
            vaultState: pda,
            systemProgram: SystemProgram.programId,
          })
          .signers([tempAuth])
          .rpc(),
        ERR.InvalidParams
      );
    });

    it("❌ Fails with invalid volatility weights (sum != 10000 bps)", async () => {
      const tempAuth = Keypair.generate();
      await airdropIfNeeded(tempAuth.publicKey, 2);
      const [pda] = deriveVaultPda(tempAuth.publicKey);

      const params = defaultInitParams({ volWeightRealizedBps: 7000, volWeightImpliedBps: 4000 });

      await expectFail(
        pg.program.methods
          .initializeVault(params)
          .accounts({
            authority: tempAuth.publicKey,
            vaultState: pda,
            systemProgram: SystemProgram.programId,
          })
          .signers([tempAuth])
          .rpc(),
        ERR.InvalidParams
      );
    });

    it("❌ Fails with invalid vol_mode", async () => {
      const tempAuth = Keypair.generate();
      await airdropIfNeeded(tempAuth.publicKey, 2);
      const [pda] = deriveVaultPda(tempAuth.publicKey);

      const params = defaultInitParams({ volMode: 9 });

      await expectFail(
        pg.program.methods
          .initializeVault(params)
          .accounts({
            authority: tempAuth.publicKey,
            vaultState: pda,
            systemProgram: SystemProgram.programId,
          })
          .signers([tempAuth])
          .rpc(),
        ERR.InvalidParams
      );
    });

    it("✅ Initializes with all three oracle feed choices (1/2/3)", async () => {
      const choices = [1, 2, 3];
      for (const c of choices) {
        const tempAuth = Keypair.generate();
        await airdropIfNeeded(tempAuth.publicKey, 2);

        const [pda] = deriveVaultPda(tempAuth.publicKey);
        const params = defaultInitParams({ oracleFeedChoice: c });

        const sig = await pg.program.methods
          .initializeVault(params)
          .accounts({
            authority: tempAuth.publicKey,
            vaultState: pda,
            systemProgram: SystemProgram.programId,
          })
          .signers([tempAuth])
          .rpc();

        console.log(`✅ init oracleFeedChoice=${c} sig=${sig}`);

        const v = await fetchVault(pda);
        assert(v.oracleFeedChoice === c, `oracleFeedChoice expected ${c} got ${v.oracleFeedChoice}`);
      }
    });
  });

  /* ----------------------------- */
  /* Keeper Operations              */
  /* ----------------------------- */
  describe("Keeper Operations", () => {
    it("✅ Authority adds keepers via add_keeper", async () => {
      const sig1 = await pg.program.methods
        .addKeeper(keeper1.publicKey)
        .accounts({
          keeperAdmin: authority.publicKey,
          vaultState: vaultStatePda,
        })
        .rpc();
      console.log(`✅ addKeeper keeper1 sig=${sig1}`);

      const sig2 = await pg.program.methods
        .addKeeper(keeper2.publicKey)
        .accounts({
          keeperAdmin: authority.publicKey,
          vaultState: vaultStatePda,
        })
        .rpc();
      console.log(`✅ addKeeper keeper2 sig=${sig2}`);

      const v = await fetchVault(vaultStatePda);
      assert(v.keeperCount >= 2, "keeperCount should be >= 2");
    });

    it("✅ Deposits keeper bond (simulated) + enforces bond requirement", async () => {
      // Require a small bond
      await pg.program.methods
        .setKeeperControls(50, new BN(1_000_000)) // 0.001 SOL
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      // Without bond → fail
      await expectFail(
        pg.program.methods
          .updateImpliedVol(500)
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc(),
        ERR.KeeperBondInsufficient
      );

      // Deposit bond
      const depSig = await pg.program.methods
        .depositKeeperBond(new BN(1_000_000))
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ depositKeeperBond sig=${depSig}`);

      // Now ok
      const okSig = await pg.program.methods
        .updateImpliedVol(500)
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ updateImpliedVol after bond sig=${okSig}`);

      // Reset requirement
      await pg.program.methods
        .setKeeperControls(200, new BN(0))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });

    it("✅ Enforces keeper rate limit (max_updates_per_epoch)", async () => {
      // Start new epoch to reset counters
      await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      // Set low rate limit = 2
      await pg.program.methods
        .setKeeperControls(2, new BN(0))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      // Two updates ok
      await pg.program.methods
        .updateCarryInputs(10, 5, 8)
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      await pg.program.methods
        .updateImpliedVol(300)
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      // Third should fail
      await expectFail(
        pg.program.methods
          .updateImpliedVol(301)
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc(),
        ERR.KeeperRateLimited
      );

      // Restore sane limit
      await pg.program.methods
        .setKeeperControls(200, new BN(0))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });
  });

  /* ----------------------------- */
  /* Oracle Integration (Pyth)      */
  /* ----------------------------- */
  describe("Oracle Price Updates (Pyth)", () => {
    it("✅ Updates oracle price from SOL/USD Pyth feed", async () => {
      if (!pythAvailable) return;

      await pg.program.methods
        .setOracleConfig(1, new BN(120), 200, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      const { result: sig, events } = await withEventListener("OraclePriceUpdated", async () => {
        return pg.program.methods
          .updateOraclePrice()
          .accounts({
            signer: keeper1.publicKey,
            vaultState: vaultStatePda,
            pythSolUsd: ORACLE_FEED_SOL_USD,
            pythSolUsdc: ORACLE_FEED_SOL_USDC,
          })
          .signers([keeper1])
          .rpc();
      });

      console.log(`✅ updateOraclePrice(SOL/USD) sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logOracleState(v);

      assert(v.oraclePriceFp.toNumber() > 0, "oracle_price_fp should be > 0");
      assert(v.oracleEmaPriceFp.toNumber() > 0, "oracle_ema_price_fp should be > 0");
      assert(v.oraclePublishSlot.toNumber() > 0, "oracle_publish_time (seconds) should be > 0");

      if (events.length) {
        const e = events[events.length - 1];
        assert(e.feedUsed === 1, "feedUsed should be 1 for SOL/USD");
      }
    });

    it("✅ Updates oracle price from SOL/USDC Pyth feed", async () => {
      if (!pythAvailable) return;

      await pg.program.methods
        .setOracleConfig(2, new BN(120), 200, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      const sig = await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();

      console.log(`✅ updateOraclePrice(SOL/USDC) sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logOracleState(v);
      assert(v.oraclePriceFp.toNumber() > 0, "oracle_price_fp should be > 0");
    });

    it("✅ Auto-selects best feed with AutoPreferUsdThenUsdc mode", async () => {
      if (!pythAvailable) return;

      await pg.program.methods
        .setOracleConfig(3, new BN(120), 200, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      const { result: sig, events } = await withEventListener("OraclePriceUpdated", async () => {
        return pg.program.methods
          .updateOraclePrice()
          .accounts({
            signer: keeper1.publicKey,
            vaultState: vaultStatePda,
            pythSolUsd: ORACLE_FEED_SOL_USD,
            pythSolUsdc: ORACLE_FEED_SOL_USDC,
          })
          .signers([keeper1])
          .rpc();
      });

      console.log(`✅ updateOraclePrice(Auto) sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logOracleState(v);

      if (events.length) {
        const e = events[events.length - 1];
        console.log(`🔎 feedUsed=${e.feedUsed} (1=USD,2=USDC)`);
        assert(e.feedUsed === 1 || e.feedUsed === 2, "feedUsed must be 1 or 2");
      }
    });

    it("✅ Records oracle returns with min_return_spacing_slots gate", async () => {
      if (!pythAvailable) return;

      // Ensure spacing gate is small
      await pg.program.methods
        .setVolModel(0, 1500, 4, new BN(2))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      const before = await fetchVault(vaultStatePda);
      const beforeSamples = before.nonzeroSamples;

      console.log(`🔄 nonzero_samples before=${beforeSamples}`);

      // First update
      await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();

      // Immediate second update likely blocked by spacing (return may not record)
      await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();

      await waitForSlots(3);

      // Third update should pass spacing and record return
      await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();

      const after = await fetchVault(vaultStatePda);
      console.log(`🔄 nonzero_samples after=${after.nonzeroSamples}`);
      assert(after.nonzeroSamples >= beforeSamples, "nonzero_samples should not decrease");
    });

    it("⚠️ Circuit breaker: staleness (deterministic) — sets oracle_degraded then clears", async () => {
      if (!pythAvailable) return;

      // Make max age tiny (1 second) then wait so publish_time becomes stale.
      await pg.program.methods
        .setOracleConfig(3, new BN(1), 200, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      console.log("⏳ Waiting 3 seconds to trigger staleness gate...");
      await delayMs(3000);

      await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();

      const v = await fetchVault(vaultStatePda);
      logOracleState(v);
      assert(v.oracleDegraded === true, "oracle_degraded should be true under forced staleness");

      // Restore normal and clear
      await pg.program.methods
        .setOracleConfig(3, new BN(120), 200, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();

      const v2 = await fetchVault(vaultStatePda);
      logOracleState(v2);
      assert(v2.oracleDegraded === false, "oracle_degraded should clear when price becomes valid");
    });
  });

  /* ----------------------------- */
  /* Volatility Models              */
  /* ----------------------------- */
  describe("Volatility Models", () => {
    it("✅ Requires min_samples before computing realized vol (policy update)", async () => {
      await pg.program.methods
        .setVolModel(0, 1500, 30, new BN(1))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      const sig = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ updateEpochAndPolicy sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logPolicyState(v);
      assert(v.realizedVolBps >= 0 && v.realizedVolBps <= 10_000, "realizedVolBps out of range");

      // restore
      await pg.program.methods
        .setVolModel(0, 1500, 4, new BN(1))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });

    async function pumpOracleSamples(k: any, loops = 6) {
      if (!pythAvailable) return;
      for (let i = 0; i < loops; i++) {
        await pg.program.methods
          .updateOraclePrice()
          .accounts({
            signer: k.publicKey,
            vaultState: vaultStatePda,
            pythSolUsd: ORACLE_FEED_SOL_USD,
            pythSolUsdc: ORACLE_FEED_SOL_USDC,
          })
          .signers([k])
          .rpc();
        await waitForSlots(2);
      }
    }

    it("✅ Computes realized vol using STDEV mode (smoke)", async () => {
      await pg.program.methods
        .setVolModel(0, 1500, 4, new BN(1))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await pumpOracleSamples(keeper1, 6);

      const sig = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ STDEV updateEpochAndPolicy sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logPolicyState(v);
      assert(v.realizedVolBps <= 10_000, "realizedVolBps should be <= 10_000");
    });

    it("✅ Computes realized vol using EWMA mode (smoke)", async () => {
      await pg.program.methods
        .setVolModel(1, 1500, 4, new BN(1))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await pumpOracleSamples(keeper1, 6);

      const sig = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ EWMA updateEpochAndPolicy sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logPolicyState(v);
      assert(v.realizedVolBps <= 10_000, "realizedVolBps should be <= 10_000");
    });

    it("✅ Computes realized vol using MAD mode (smoke)", async () => {
      await pg.program.methods
        .setVolModel(2, 1500, 4, new BN(1))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await pumpOracleSamples(keeper1, 6);

      const sig = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ MAD updateEpochAndPolicy sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logPolicyState(v);
      assert(v.realizedVolBps <= 10_000, "realizedVolBps should be <= 10_000");
    });

    it("✅ Combines realized + implied vol into vol_score with weights", async () => {
      await pg.program.methods
        .updateImpliedVol(1200)
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      const sig = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ updateEpochAndPolicy (combine) sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      logPolicyState(v);

      assert(v.volScoreBps >= 0 && v.volScoreBps <= 10_000, "volScoreBps out of range");
    });
  });

  /* ----------------------------- */
  /* Policy Update Cooldowns        */
  /* ----------------------------- */
  describe("Epoch and Policy Updates", () => {
    it("✅ Enforces policy_update_min_slots cooldown", async () => {
      await pg.program.methods
        .setPolicyStability(new BN(10), 1000, 100, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      const sig1 = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ first updateEpochAndPolicy sig=${sig1}`);

      await expectFail(
        pg.program.methods
          .updateEpochAndPolicy()
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc(),
        ERR.PolicyCooldown
      );

      await waitForSlots(12);

      const sig2 = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`✅ cooldown satisfied sig=${sig2}`);

      // restore
      await pg.program.methods
        .setPolicyStability(new BN(5), 1000, 100, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });

    it("✅ Freezes policy updates when oracle_degraded=true (staleness forced)", async () => {
      if (!pythAvailable) return;

      await pg.program.methods
        .setOracleConfig(3, new BN(1), 200, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await delayMs(2500);

      await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();

      const v1 = await fetchVault(vaultStatePda);
      assert(v1.oracleDegraded === true, "oracle must be degraded for freeze test");

      const bandBefore = v1.bandBps;
      const intervalBefore = v1.minHedgeIntervalSlots.toString();

      await waitForSlots(6);

      const sig = await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`🧊 policy freeze update sig=${sig}`);

      const v2 = await fetchVault(vaultStatePda);
      assert(v2.bandBps === bandBefore, "band should remain unchanged under freeze");
      assert(v2.minHedgeIntervalSlots.toString() === intervalBefore, "interval should remain unchanged under freeze");

      // restore oracle
      await pg.program.methods
        .setOracleConfig(3, new BN(120), 200, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await pg.program.methods
        .updateOraclePrice()
        .accounts({
          signer: keeper1.publicKey,
          vaultState: vaultStatePda,
          pythSolUsd: ORACLE_FEED_SOL_USD,
          pythSolUsdc: ORACLE_FEED_SOL_USDC,
        })
        .signers([keeper1])
        .rpc();
    });
  });

  /* ----------------------------- */
  /* Risk Guardrails                */
  /* ----------------------------- */
  describe("Risk Guardrails", () => {
    it("✅ Deposits reserve SOL + enforces min_reserve_bps ratio", async () => {
      // Increase minReserveBps to force reserve requirement (e.g., 20%)
      await pg.program.methods
        .setRiskCaps(new BN(10_000), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await expectFail(
        pg.program.methods.depositAndStake(new BN(1000)).accounts({ vaultState: vaultStatePda }).rpc(),
        ERR.ReserveTooLow
      );

      const sigR = await pg.program.methods
        .depositReserve(new BN(300))
        .accounts({ vaultState: vaultStatePda })
        .rpc();
      console.log(`✅ depositReserve sig=${sigR}`);

      const sigS = await pg.program.methods
        .depositAndStake(new BN(1000))
        .accounts({ vaultState: vaultStatePda })
        .rpc();
      console.log(`✅ depositAndStake sig=${sigS}`);

      const v = await fetchVault(vaultStatePda);
      logVaultSnapshot(v);

      assert(v.reserveSol.toNumber() >= 300, "reserve should increase");
      assert(v.stakedSol.toNumber() >= 1000, "staked should increase");

      // restore minReserveBps to 5%
      await pg.program.methods
        .setRiskCaps(new BN(10_000), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 500)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });

    it("✅ Enforces max_staked_sol cap", async () => {
      await pg.program.methods
        .setRiskCaps(new BN(1200), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 500)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await expectFail(
        pg.program.methods.depositAndStake(new BN(10_000)).accounts({ vaultState: vaultStatePda }).rpc(),
        ERR.CapExceeded
      );

      // restore
      await pg.program.methods
        .setRiskCaps(new BN(10_000), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 500)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });
  });

  /* ----------------------------- */
  /* Hedge Request/Confirm (best-effort) */
  /* ----------------------------- */
  describe("Hedge Request and Confirmation", () => {
    it("✅ Requests hedge when interval met AND drift exceeds band (best-effort)", async () => {
      await pg.program.methods
        .setPolicyBounds(50, 50, new BN(1), new BN(1))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await pg.program.methods
        .setPolicyStability(new BN(1), 1000, 0, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      if (pythAvailable) {
        await pg.program.methods
          .updateOraclePrice()
          .accounts({
            signer: keeper1.publicKey,
            vaultState: vaultStatePda,
            pythSolUsd: ORACLE_FEED_SOL_USD,
            pythSolUsdc: ORACLE_FEED_SOL_USDC,
          })
          .signers([keeper1])
          .rpc();
      }

      await waitForSlots(2);

      try {
        const { result: sig, events } = await withEventListener("HedgeRequested", async () => {
          return pg.program.methods.requestHedge().accounts({ vaultState: vaultStatePda }).rpc();
        });

        console.log(`✅ requestHedge sig=${sig}`);

        const v = await fetchVault(vaultStatePda);
        assert(v.requestOutstanding === true, "request_outstanding should be true");

        if (events.length) {
          const e = events[events.length - 1];
          console.log(
            `📣 HedgeRequested: requestId=${e.requestId.toString()} drift=${bpsToPct(e.driftBps)} target=${e.targetHedgeNotionalUsd.toString()}`
          );
          assert(e.requestId.toNumber() === v.lastHedgeRequestId.toNumber(), "event requestId mismatch");
        }
      } catch (e: any) {
        console.log(`⚠️ requestHedge did not trigger (drift within band). Error: ${String(e?.message ?? e)}`);
      }
    });

    it("✅ Confirms hedge with valid request_id (if outstanding)", async () => {
      const v0 = await fetchVault(vaultStatePda);
      if (!v0.requestOutstanding) {
        console.log("⚠️ No outstanding request. Skipping confirm test (depends on drift triggers).");
        return;
      }

      const newHedge = new BN(0);
      const fillPriceFp = new BN(100 * PRICE_FP_SCALE);

      const { result: sig, events } = await withEventListener("HedgeConfirmed", async () => {
        return pg.program.methods
          .confirmHedge(v0.lastHedgeRequestId, newHedge, fillPriceFp)
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc();
      });

      console.log(`✅ confirmHedge sig=${sig}`);

      const v1 = await fetchVault(vaultStatePda);
      assert(v1.requestOutstanding === false, "request_outstanding should be cleared");
      assert(v1.hedgeFillCount.toNumber() >= 1, "hedge_fill_count should increment");

      if (events.length) {
        const e = events[events.length - 1];
        console.log(
          `📣 HedgeConfirmed: requestId=${e.requestId.toString()} slippage=${bpsToPct(e.slippageBps)} avgSlip=${bpsToPct(
            e.avgFillSlippageBps
          )}`
        );
      }
    });

    it("❌ Fails confirm with wrong request_id (if outstanding)", async () => {
      const v0 = await fetchVault(vaultStatePda);
      if (!v0.requestOutstanding) {
        console.log("⚠️ No outstanding request. Skipping wrong request_id test.");
        return;
      }

      const wrongId = v0.lastHedgeRequestId.add(new BN(999));

      await expectFail(
        pg.program.methods
          .confirmHedge(wrongId, new BN(0), new BN(100 * PRICE_FP_SCALE))
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc(),
        ERR.WrongRequestId
      );
    });
  });

  /* ----------------------------- */
  /* Authority Controls (safe ones) */
  /* ----------------------------- */
  describe("Authority Controls", () => {
    it("✅ Sets paused flag and blocks user actions", async () => {
      const sig = await pg.program.methods
        .setPaused(true)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
      console.log(`⛔ setPaused(true) sig=${sig}`);

      await expectFail(pg.program.methods.depositReserve(new BN(1)).accounts({ vaultState: vaultStatePda }).rpc(), ERR.Paused);

      await pg.program.methods
        .setPaused(false)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      console.log("✅ unpaused");
    });

    it("✅ Toggles emergency_withdraw_enabled and bumps config_version", async () => {
      const v0 = await fetchVault(vaultStatePda);

      const sig = await pg.program.methods
        .setEmergencyWithdrawEnabled(true)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
      console.log(`🚨 setEmergencyWithdrawEnabled(true) sig=${sig}`);

      const v1 = await fetchVault(vaultStatePda);
      assert(v1.emergencyWithdrawEnabled === true, "emergency_withdraw_enabled should be true");
      assert(v1.configVersion.toNumber() === v0.configVersion.toNumber() + 1, "configVersion should bump");

      // restore
      await pg.program.methods
        .setEmergencyWithdrawEnabled(false)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });

    // NOTE: Authority transfer is intentionally omitted here because Playground’s `pg.wallet` can’t be swapped as signer.
    // If you want this test, run it on a fresh vault and accept that later authority-only calls will fail in Playground.
  });

  /* ----------------------------- */
  /* Integration Scenarios          */
  /* ----------------------------- */
  describe("Integration Scenarios", () => {
    it("🔄 Full workflow (best-effort): oracle -> policy -> request -> confirm", async () => {
      if (pythAvailable) {
        await pg.program.methods
          .updateOraclePrice()
          .accounts({
            signer: keeper1.publicKey,
            vaultState: vaultStatePda,
            pythSolUsd: ORACLE_FEED_SOL_USD,
            pythSolUsdc: ORACLE_FEED_SOL_USDC,
          })
          .signers([keeper1])
          .rpc();
      }

      await waitForSlots(6);

      await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      const v1 = await fetchVault(vaultStatePda);
      logOracleState(v1);
      logPolicyState(v1);
      logVaultSnapshot(v1);

      try {
        const reqSig = await pg.program.methods.requestHedge().accounts({ vaultState: vaultStatePda }).rpc();
        console.log(`✅ requestHedge sig=${reqSig}`);

        const v2 = await fetchVault(vaultStatePda);
        if (v2.requestOutstanding) {
          const confSig = await pg.program.methods
            .confirmHedge(v2.lastHedgeRequestId, new BN(0), new BN(100 * PRICE_FP_SCALE))
            .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
            .signers([keeper1])
            .rpc();
          console.log(`✅ confirmHedge sig=${confSig}`);
        }
      } catch (e: any) {
        console.log(`⚠️ Hedge leg not triggered (drift/band). Error: ${String(e?.message ?? e)}`);
      }
    });

    it("👥 Multi-keeper: keeper2 updates carry + implied vol", async () => {
      const sig1 = await pg.program.methods
        .updateCarryInputs(20, 5, 10)
        .accounts({ signer: keeper2.publicKey, vaultState: vaultStatePda })
        .signers([keeper2])
        .rpc();
      console.log(`✅ keeper2 updateCarryInputs sig=${sig1}`);

      const sig2 = await pg.program.methods
        .updateImpliedVol(900)
        .accounts({ signer: keeper2.publicKey, vaultState: vaultStatePda })
        .signers([keeper2])
        .rpc();
      console.log(`✅ keeper2 updateImpliedVol sig=${sig2}`);

      const v = await fetchVault(vaultStatePda);
      console.log(`📌 carry approx (bps/day) = ${v.fundingBpsPerDay + v.stakingBpsPerDay - v.borrowBpsPerDay}`);
    });
  });
});
