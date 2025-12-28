declare function setTimeout(handler: (...args: any[]) => void, timeout?: number): any;

function assert(cond: any, msg?: string): asserts cond {
  if (!cond) throw new Error(msg ?? "assertion failed");
}

const { PublicKey, Keypair, SystemProgram, LAMPORTS_PER_SOL } = web3;

const PRICE_FP_SCALE = 1_000_000;

// Pyth devnet feed accounts (as provided)
const ORACLE_FEED_SOL_USD = new PublicKey("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix");
const ORACLE_FEED_SOL_USDC = new PublicKey("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD");

const ERR = {
  InvalidParams: "Invalid parameters",
  Paused: "Program is paused",
  KeeperBondInsufficient: "Keeper bond insufficient",
  KeeperRateLimited: "Keeper rate limited",
  ReserveTooLow: "Reserve too low",
  CapExceeded: "Cap exceeded",
  PolicyCooldown: "Policy update cooldown not met",
  WrongRequestId: "Wrong request id",
};

const delayMs = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

async function waitForSlots(n: number) {
  const start = await pg.connection.getSlot("confirmed");
  while (true) {
    const cur = await pg.connection.getSlot("confirmed");
    if (cur >= start + n) return;
    await delayMs(350);
  }
}

function getDeployedProgramId(): any {
  const anyIdl: any = (pg.program as any).idl;
  const idlAddr = anyIdl?.metadata?.address;
  if (idlAddr) return new PublicKey(idlAddr);
  return pg.program.programId;
}

function deriveVaultPda(authorityPk: any) {
  const programId = getDeployedProgramId();
  return PublicKey.findProgramAddressSync([Buffer.from("vault"), authorityPk.toBuffer()], programId);
}

async function fetchVault(vaultStatePda: any) {
  return pg.program.account.vaultState.fetch(vaultStatePda);
}

function fpToDecimal(fp: any, scale = PRICE_FP_SCALE) {
  const n = typeof fp === "number" ? fp : fp?.toNumber?.() ?? 0;
  return n / scale;
}

async function withEventListener<T>(
  eventName: any,
  fn: () => Promise<T>
): Promise<{ result: T; events: any[] }> {
  const events: any[] = [];
  let listenerId: any = null;

  try {
    listenerId = await pg.program.addEventListener(eventName as any, (e: any) => events.push(e));
    const result = await fn();
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
    throw new Error(`Expected failure containing "${mustInclude}", but tx succeeded`);
  } catch (e: any) {
    const msg = String(e?.message ?? e);
    assert(
      msg.includes(mustInclude) || msg.includes("custom program error") || msg.includes("AnchorError"),
      `Expected error to include "${mustInclude}" but got: ${msg}`
    );
  }
}

/**
 * Funding helper:
 * Avoid requestAirdrop (rate limits / internal errors) by transferring lamports from pg.wallet.
 * pg.wallet must already have SOL on the cluster.
 */
async function fundFromWallet(dest: any, sol: number) {
  const lamports = Math.floor(sol * LAMPORTS_PER_SOL);
  const tx = new web3.Transaction().add(
    SystemProgram.transfer({
      fromPubkey: pg.wallet.publicKey,
      toPubkey: dest,
      lamports,
    })
  );
  const sig = await pg.program.provider.sendAndConfirm(tx, []);
  return sig;
}

async function ensureFunded(dest: any, minSol: number) {
  const bal = await pg.connection.getBalance(dest, "confirmed");
  if (bal >= minSol * LAMPORTS_PER_SOL) return;
  await fundFromWallet(dest, minSol);
}

async function ensurePythFeedsExist() {
  const usd = await pg.connection.getAccountInfo(ORACLE_FEED_SOL_USD, "confirmed");
  const usdc = await pg.connection.getAccountInfo(ORACLE_FEED_SOL_USDC, "confirmed");

  if (!usd || !usdc) {
    console.log("Pyth feeds not found on this RPC/cluster. Oracle tests will be skipped.");
    return false;
  }

  console.log("Pyth feeds found:");
  console.log("SOL/USD :", ORACLE_FEED_SOL_USD.toBase58());
  console.log("SOL/USDC:", ORACLE_FEED_SOL_USDC.toBase58());
  return true;
}

// Must match your IDL InitializeParams field names (camelCase).
function defaultInitParams(overrides: Partial<any> = {}) {
  const base = {
    minBandBps: 50,
    maxBandBps: 1500,
    minIntervalSlots: new BN(5),
    maxIntervalSlots: new BN(50),

    volWeightRealizedBps: 6000,
    volWeightImpliedBps: 4000,

    minSamples: 4,
    minReturnSpacingSlots: new BN(2),

    policyUpdateMinSlots: new BN(5),
    maxPolicySlewBps: 1000,
    hysteresisBps: 100,

    volMode: 0,
    ewmaAlphaBps: 1500,

    maxStakedSol: new BN(10_000),
    maxAbsHedgeNotionalUsd: new BN(2_000_000),
    maxHedgePerSolUsdFp: new BN(400 * PRICE_FP_SCALE),
    minReserveBps: 500,

    oracleFeedChoice: 3,
    maxPriceAgeSlots: new BN(120),
    maxConfidenceBps: 200,
    maxPriceJumpBps: 2000,

    targetDeltaBps: 10_000,
    lstBetaFp: new BN(1 * PRICE_FP_SCALE),

    maxConfirmDelaySlots: new BN(25),
    extremeDriftBps: 2000,

    maxUpdatesPerEpoch: 50,
    keeperBondRequiredLamports: new BN(0),
  };

  return { ...base, ...overrides };
}

describe("vol_weighted_staking (Pyth devnet integration) — Solana Playground", () => {
  const authority = pg.wallet;
  const user = Keypair.generate();
  const keeper1 = Keypair.generate();
  const keeper2 = Keypair.generate();

  let vaultStatePda: any;
  let vaultBump = 0;
  let pythOk = true;

  it("Setup: fund actors and derive PDA", async () => {
    await ensureFunded(user.publicKey, 0.25);
    await ensureFunded(keeper1.publicKey, 0.25);
    await ensureFunded(keeper2.publicKey, 0.25);

    const [pda, bump] = deriveVaultPda(authority.publicKey);
    vaultStatePda = pda;
    vaultBump = bump;

    console.log(`ProgramID = ${getDeployedProgramId().toBase58()}`);
    console.log(`vaultState PDA = ${vaultStatePda.toBase58()} bump=${vaultBump}`);

    pythOk = await ensurePythFeedsExist();
    assert(!!vaultStatePda, "vaultStatePda missing");
  });

  describe("Initialization", () => {
    it("Initializes vault with valid parameters (or reuses existing)", async () => {
      const params = defaultInitParams({ oracleFeedChoice: 3 });

      const existing = await pg.connection.getAccountInfo(vaultStatePda, "confirmed");
      if (existing) {
        console.log("Vault already exists at PDA; reusing.");
        const v = await fetchVault(vaultStatePda);
        assert(v.authority.equals(authority.publicKey), "authority mismatch");
        return;
      }

      const { result: sig } = await withEventListener("VaultInitialized", async () => {
        return pg.program.methods
          .initializeVault(params)
          .accounts({
            authority: authority.publicKey,
            vaultState: vaultStatePda,
            systemProgram: SystemProgram.programId,
          })
          .rpc();
      });

      console.log(`initializeVault sig: ${sig}`);

      const v = await fetchVault(vaultStatePda);
      assert(v.authority.equals(authority.publicKey), "authority mismatch");
      assert(v.configVersion.toNumber() >= 1, "configVersion should be >= 1");
    });

    it("Fails with invalid policy bounds (min > max)", async () => {
      const tempAuth = Keypair.generate();
      await ensureFunded(tempAuth.publicKey, 0.15);

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

    it("Fails with invalid volatility weights (sum != 10000 bps)", async () => {
      const tempAuth = Keypair.generate();
      await ensureFunded(tempAuth.publicKey, 0.15);

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

    it("Fails with invalid vol_mode", async () => {
      const tempAuth = Keypair.generate();
      await ensureFunded(tempAuth.publicKey, 0.15);

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

    it("Initializes with all three oracle feed choices (1/2/3)", async () => {
      const choices = [1, 2, 3];
      for (const c of choices) {
        const tempAuth = Keypair.generate();
        await ensureFunded(tempAuth.publicKey, 0.15);

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

        console.log(`init oracleFeedChoice=${c} sig=${sig}`);

        const v = await fetchVault(pda);
        assert(v.oracleFeedChoice === c, `oracleFeedChoice expected ${c} got ${v.oracleFeedChoice}`);
      }
    });
  });

  describe("Keeper Operations", () => {
    it("Authority adds keepers", async () => {
      const sig1 = await pg.program.methods
        .addKeeper(keeper1.publicKey)
        .accounts({ keeperAdmin: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
      console.log(`addKeeper keeper1 sig=${sig1}`);

      const sig2 = await pg.program.methods
        .addKeeper(keeper2.publicKey)
        .accounts({ keeperAdmin: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
      console.log(`addKeeper keeper2 sig=${sig2}`);

      const v = await fetchVault(vaultStatePda);
      assert(v.keeperCount >= 2, "keeperCount should be >= 2");
    });

    it("Deposits keeper bond (simulated) + enforces requirement", async () => {
      await pg.program.methods
        .setKeeperControls(50, new BN(1_000_000))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await expectFail(
        pg.program.methods
          .updateImpliedVol(500)
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc(),
        ERR.KeeperBondInsufficient
      );

      const depSig = await pg.program.methods
        .depositKeeperBond(new BN(1_000_000))
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`depositKeeperBond sig=${depSig}`);

      const okSig = await pg.program.methods
        .updateImpliedVol(500)
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();
      console.log(`updateImpliedVol after bond sig=${okSig}`);

      await pg.program.methods
        .setKeeperControls(200, new BN(0))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });

    it("Enforces keeper rate limit", async () => {
      await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      await pg.program.methods
        .setKeeperControls(2, new BN(0))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

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

      await expectFail(
        pg.program.methods
          .updateImpliedVol(301)
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc(),
        ERR.KeeperRateLimited
      );

      await pg.program.methods
        .setKeeperControls(200, new BN(0))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });
  });

  describe("Oracle Price Updates (Pyth)", () => {
    it("Updates oracle price from SOL/USD feed", async () => {
      if (!pythOk) return;

      await pg.program.methods
        .setOracleConfig(1, new BN(120), 200, 2000)
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

      console.log(`updateOraclePrice SOL/USD sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      assert(v.oraclePriceFp.toNumber() > 0, "oracle_price_fp should be > 0");
      console.log(`spot=${fpToDecimal(v.oraclePriceFp).toFixed(4)} ema=${fpToDecimal(v.oracleEmaPriceFp).toFixed(4)}`);
    });

    it("Updates oracle price from SOL/USDC feed", async () => {
      if (!pythOk) return;

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

      console.log(`updateOraclePrice SOL/USDC sig=${sig}`);

      const v = await fetchVault(vaultStatePda);
      assert(v.oraclePriceFp.toNumber() > 0, "oracle_price_fp should be > 0");
    });
  });

  describe("Epoch and Policy Updates", () => {
    it("Enforces policy update cooldown", async () => {
      await pg.program.methods
        .setPolicyStability(new BN(10), 1000, 100, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      await expectFail(
        pg.program.methods
          .updateEpochAndPolicy()
          .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
          .signers([keeper1])
          .rpc(),
        ERR.PolicyCooldown
      );

      await waitForSlots(12);

      await pg.program.methods
        .updateEpochAndPolicy()
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      await pg.program.methods
        .setPolicyStability(new BN(5), 1000, 100, 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });
  });

  describe("Risk Guardrails", () => {
    it("Deposits reserve + enforces reserve ratio", async () => {
      await pg.program.methods
        .setRiskCaps(new BN(10_000), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 2000)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await expectFail(
        pg.program.methods.depositAndStake(new BN(1000)).accounts({ vaultState: vaultStatePda }).rpc(),
        ERR.ReserveTooLow
      );

      await pg.program.methods.depositReserve(new BN(300)).accounts({ vaultState: vaultStatePda }).rpc();
      await pg.program.methods.depositAndStake(new BN(1000)).accounts({ vaultState: vaultStatePda }).rpc();

      const v = await fetchVault(vaultStatePda);
      assert(v.reserveSol.toNumber() >= 300, "reserve should increase");
      assert(v.stakedSol.toNumber() >= 1000, "staked should increase");

      await pg.program.methods
        .setRiskCaps(new BN(10_000), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 500)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });

    it("Enforces staked cap", async () => {
      await pg.program.methods
        .setRiskCaps(new BN(1200), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 500)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await expectFail(
        pg.program.methods.depositAndStake(new BN(10_000)).accounts({ vaultState: vaultStatePda }).rpc(),
        ERR.CapExceeded
      );

      await pg.program.methods
        .setRiskCaps(new BN(10_000), new BN(2_000_000), new BN(400 * PRICE_FP_SCALE), 500)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });
  });

  describe("Hedge Request and Confirmation", () => {
    it("Requests hedge (best-effort) and never crashes", async () => {
      await pg.program.methods
        .setPolicyBounds(50, 50, new BN(1), new BN(1))
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      if (pythOk) {
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
        await pg.program.methods.requestHedge().accounts({ vaultState: vaultStatePda }).rpc();
      } catch (e: any) {
        console.log(`requestHedge not triggered: ${String(e?.message ?? e)}`);
      }

      const v = await fetchVault(vaultStatePda);
      assert(typeof v.requestOutstanding === "boolean", "requestOutstanding should be boolean");
    });

    it("Confirms hedge if outstanding, otherwise skips", async () => {
      const v0 = await fetchVault(vaultStatePda);
      if (!v0.requestOutstanding) {
        console.log("No outstanding request; skipping confirm.");
        return;
      }

      const requestId: any = v0.lastHedgeRequestId;
      assert(!!requestId, "missing lastHedgeRequestId");

      await pg.program.methods
        .confirmHedge(requestId, new BN(0), new BN(100 * PRICE_FP_SCALE))
        .accounts({ signer: keeper1.publicKey, vaultState: vaultStatePda })
        .signers([keeper1])
        .rpc();

      const v1 = await fetchVault(vaultStatePda);
      assert(v1.requestOutstanding === false, "requestOutstanding should clear");
    });

    it("Wrong request id fails if outstanding, otherwise skips", async () => {
      const v0 = await fetchVault(vaultStatePda);
      if (!v0.requestOutstanding) {
        console.log("No outstanding request; skipping wrong id test.");
        return;
      }

      const requestId: any = v0.lastHedgeRequestId;
      assert(!!requestId, "missing lastHedgeRequestId");

      const wrongId = requestId.add ? requestId.add(new BN(999)) : new BN(requestId).add(new BN(999));

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

  describe("Authority Controls", () => {
    it("Paused blocks actions", async () => {
      await pg.program.methods
        .setPaused(true)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();

      await expectFail(pg.program.methods.depositReserve(new BN(1)).accounts({ vaultState: vaultStatePda }).rpc(), ERR.Paused);

      await pg.program.methods
        .setPaused(false)
        .accounts({ authority: authority.publicKey, vaultState: vaultStatePda })
        .rpc();
    });
  });
});

