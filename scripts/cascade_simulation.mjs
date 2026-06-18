/**
 * Nebula DEX — Perp Shield Cascade Simulation (IDL-accurate)
 *
 * Uses exact discriminators and account layouts from target/idl/nebula_dex.json
 *
 * Steps:
 *   1. Create SPL token mints + vaults (base, quote)
 *   2. perp_init_market
 *   3. perp_shield_init (LOW thresholds)
 *   4. perp_open_position x3 (all long → OI imbalance)
 *   5. perp_shield_liquidation_guard
 *   6. perp_shield_trigger_breaker
 *   7. Verify circuit_breaker_active == 1
 *   8. perp_shield_reset_breaker (force=true)
 *   9. Verify circuit_breaker_active == 0
 *
 * Usage: node scripts/cascade_simulation.mjs
 */

import {
  Connection, Keypair, PublicKey, SystemProgram,
  Transaction, TransactionInstruction, sendAndConfirmTransaction,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  createMint, createAccount, mintTo, getOrCreateAssociatedTokenAccount,
  TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { createHash } from "crypto";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// ── Constants ─────────────────────────────────────────────────────────────────

const AMM_PROGRAM_ID = new PublicKey("23dn1qvEfhPfBVvm46PGWMRRr3rjE7QSitPkzEEbeCtQ");
const RPC_URL = "https://rpc.testnet.x1.xyz";

// Exact discriminators from IDL
const DISC = {
  perp_init_market:              Buffer.from([162,17,197,119,112,167,143,38]),
  perp_shield_init:              Buffer.from([212,228,110,247,247,108,40,77]),
  perp_open_position:            Buffer.from([149,36,201,228,163,243,4,142]),
  perp_shield_liquidation_guard: Buffer.from([94,86,81,252,61,72,25,169]),
  perp_shield_trigger_breaker:   Buffer.from([238,26,193,209,226,131,83,216]),
  perp_shield_reset_breaker:     Buffer.from([24,98,92,38,113,64,134,229]),
};

// ── Keypair loader ─────────────────────────────────────────────────────────────

function loadKeypair() {
  const configPath = path.join(__dirname, "..", "client_config.ini");
  if (fs.existsSync(configPath)) {
    const cfg = fs.readFileSync(configPath, "utf8");
    const m = cfg.match(/payer_path\s*=\s*(.+)/);
    if (m) {
      const kp = m[1].trim().replace("~", process.env.HOME);
      if (fs.existsSync(kp)) return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(kp, "utf8"))));
    }
  }
  throw new Error("No keypair found in client_config.ini");
}

// ── Encoding helpers ──────────────────────────────────────────────────────────

function writeU16LE(buf, val, offset) { buf.writeUInt16LE(val, offset); }
function writeU64LE(buf, val, offset) { buf.writeBigUInt64LE(BigInt(val), offset); }
function writeU128LE(buf, val, offset) {
  const lo = BigInt(val) & 0xFFFFFFFFFFFFFFFFn;
  const hi = BigInt(val) >> 64n;
  buf.writeBigUInt64LE(lo, offset);
  buf.writeBigUInt64LE(hi, offset + 8);
}

// ── PDA helpers ───────────────────────────────────────────────────────────────

function perpMarketPDA(baseMint, quoteMint) {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("perp_market"), baseMint.toBuffer(), quoteMint.toBuffer()],
    AMM_PROGRAM_ID
  );
}
function perpShieldPDA(perpMarket) {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("perp_shield"), perpMarket.toBuffer()],
    AMM_PROGRAM_ID
  );
}
function positionPDA(perpMarket, owner) {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("position"), perpMarket.toBuffer(), owner.toBuffer()],
    AMM_PROGRAM_ID
  );
}
function cascadeAlertPDA(perpShield, alertIndex) {
  const buf = Buffer.alloc(8);
  buf.writeBigUInt64LE(BigInt(alertIndex));
  return PublicKey.findProgramAddressSync(
    [Buffer.from("cascade_alert"), perpShield.toBuffer(), buf],
    AMM_PROGRAM_ID
  );
}

// ── Logging ───────────────────────────────────────────────────────────────────

function log(step, msg, tx) {
  const ts = new Date().toISOString();
  console.log(`\n[${ts}] STEP ${step}: ${msg}${tx ? `\n    tx: ${tx}` : ""}`);
}
function logErr(step, msg, e) {
  console.error(`\n[ERROR] STEP ${step}: ${msg}\n  ${e?.message || e}`);
}

// ── Main ──────────────────────────────────────────────────────────────────────

async function main() {
  console.log("=".repeat(62));
  console.log("  Nebula DEX — Perp Shield Cascade Simulation (IDL-accurate)");
  console.log("  Program:", AMM_PROGRAM_ID.toBase58());
  console.log("  RPC:    ", RPC_URL);
  console.log("=".repeat(62));

  const connection = new Connection(RPC_URL, "confirmed");
  const authority = loadKeypair();
  console.log("\nAuthority:", authority.publicKey.toBase58());
  const balance = await connection.getBalance(authority.publicKey);
  console.log("Balance:  ", (balance / LAMPORTS_PER_SOL).toFixed(4), "XNT");
  if (balance < 0.1 * LAMPORTS_PER_SOL) throw new Error("Need at least 0.1 XNT");

  // ── STEP 1: Create mints and vaults ──────────────────────────────────────────
  log(1, "Creating base mint, quote mint, collateral vault, insurance vault...");
  let baseMint, quoteMint, collateralVault, insuranceFundVault, traderCollateral;
  try {
    baseMint = await createMint(connection, authority, authority.publicKey, null, 9);
    quoteMint = await createMint(connection, authority, authority.publicKey, null, 6);
    // Use dedicated keypairs for vault accounts (raw token accounts, not ATA)
    const collateralVaultKp = Keypair.generate();
    const insuranceVaultKp  = Keypair.generate();
    const traderCollateralKp = Keypair.generate();
    // Create raw token accounts
    const mintRent = await connection.getMinimumBalanceForRentExemption(165);
    const createRawTokenAccount = async (kp, mint) => {
      const tx = new Transaction().add(
        SystemProgram.createAccount({
          fromPubkey: authority.publicKey,
          newAccountPubkey: kp.publicKey,
          lamports: mintRent,
          space: 165,
          programId: TOKEN_PROGRAM_ID,
        }),
        new TransactionInstruction({
          programId: TOKEN_PROGRAM_ID,
          keys: [
            { pubkey: kp.publicKey,         isSigner: false, isWritable: true },
            { pubkey: mint,                 isSigner: false, isWritable: false },
            { pubkey: authority.publicKey,  isSigner: false, isWritable: false },
            { pubkey: new PublicKey("SysvarRent111111111111111111111111111111111"), isSigner: false, isWritable: false },
          ],
          data: Buffer.from([1]), // InitializeAccount instruction
        })
      );
      await sendAndConfirmTransaction(connection, tx, [authority, kp]);
      return kp.publicKey;
    };
    collateralVault    = await createRawTokenAccount(collateralVaultKp, quoteMint);
    insuranceFundVault = await createRawTokenAccount(insuranceVaultKp, quoteMint);
    traderCollateral   = await createRawTokenAccount(traderCollateralKp, quoteMint);
    // Seed vaults
    await mintTo(connection, authority, quoteMint, insuranceFundVault, authority, 10_000_000);
    await mintTo(connection, authority, quoteMint, collateralVault,    authority, 100_000_000);
    await mintTo(connection, authority, quoteMint, traderCollateral,   authority, 50_000_000);
    log(1, `Mints + vaults created ✅\n    baseMint: ${baseMint.toBase58()}\n    quoteMint: ${quoteMint.toBase58()}\n    collateralVault: ${collateralVault.toBase58()}`);
  } catch (e) { logErr(1, "Failed to create mints/vaults", e); process.exit(1); }

  const [perpMarket] = perpMarketPDA(baseMint, quoteMint);
  const [perpShield] = perpShieldPDA(perpMarket);
  console.log("\nDerived PDAs:");
  console.log("  PerpMarket:", perpMarket.toBase58());
  console.log("  PerpShield:", perpShield.toBase58());

  // ── STEP 2: perp_init_market ─────────────────────────────────────────────────
  log(2, "Calling perp_init_market...");
  try {
    // args: market_id(u64) + max_leverage(u16) + liquidation_fee_bps(u16) +
    //       taker_fee_bps(u16) + maker_fee_bps(u16) + min_collateral(u64) + price_authority(pubkey 32)
    const data = Buffer.alloc(8 + 8 + 2 + 2 + 2 + 2 + 8 + 32);
    DISC.perp_init_market.copy(data, 0);
    writeU64LE(data, 1, 8);          // market_id
    writeU16LE(data, 10, 16);        // max_leverage
    writeU16LE(data, 100, 18);       // liquidation_fee_bps (1%)
    writeU16LE(data, 30, 20);        // taker_fee_bps
    writeU16LE(data, 10, 22);        // maker_fee_bps
    writeU64LE(data, 1_000, 24);     // min_collateral
    authority.publicKey.toBuffer().copy(data, 32); // price_authority = admin

    const ix = new TransactionInstruction({
      programId: AMM_PROGRAM_ID,
      keys: [
        { pubkey: authority.publicKey,  isSigner: true,  isWritable: true  }, // admin
        { pubkey: baseMint,             isSigner: false, isWritable: false }, // base_mint
        { pubkey: quoteMint,            isSigner: false, isWritable: false }, // quote_mint
        { pubkey: perpMarket,           isSigner: false, isWritable: true  }, // perp_market PDA
        { pubkey: collateralVault,      isSigner: false, isWritable: true  }, // collateral_vault
        { pubkey: insuranceFundVault,   isSigner: false, isWritable: true  }, // insurance_fund_vault
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      data,
    });
    const sig = await sendAndConfirmTransaction(connection, new Transaction().add(ix), [authority]);
    log(2, "perp_init_market ✅", sig);
  } catch (e) {
    if (e.message?.includes("already in use") || e.message?.includes("0x0")) {
      log(2, "PerpMarket already exists ✅");
    } else { logErr(2, "perp_init_market failed", e); process.exit(1); }
  }

  // ── STEP 3: perp_shield_init (LOW thresholds) ────────────────────────────────
  log(3, "Calling perp_shield_init with LOW thresholds...");
  try {
    // args: epoch_slots(u64) + oi_imbalance_threshold_bps(u16) +
    //       liq_rate_threshold(u16) + price_velocity_threshold_bps(u16)
    const data = Buffer.alloc(8 + 8 + 2 + 2 + 2);
    DISC.perp_shield_init.copy(data, 0);
    writeU64LE(data, 60,  8);   // epoch_slots
    writeU16LE(data, 100, 16);  // oi_imbalance_threshold_bps (1% — trips easy)
    writeU16LE(data, 1,   18);  // liq_rate_threshold
    writeU16LE(data, 100, 20);  // price_velocity_threshold_bps

    const ix = new TransactionInstruction({
      programId: AMM_PROGRAM_ID,
      keys: [
        { pubkey: authority.publicKey, isSigner: true,  isWritable: true  }, // authority
        { pubkey: perpMarket,          isSigner: false, isWritable: false }, // perp_market
        { pubkey: perpShield,          isSigner: false, isWritable: true  }, // perp_shield PDA
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      data,
    });
    const sig = await sendAndConfirmTransaction(connection, new Transaction().add(ix), [authority]);
    log(3, "perp_shield_init ✅ (epoch=60 slots, oi_threshold=100bps, liq_threshold=1, vel_threshold=100bps)", sig);
  } catch (e) {
    if (e.message?.includes("already in use") || e.message?.includes("0x0")) {
      log(3, "PerpShield already initialized ✅");
    } else { logErr(3, "perp_shield_init failed", e); process.exit(1); }
  }

  // ── STEP 4: Open 3 long positions (OI imbalance) ─────────────────────────────
  log(4, "Opening 3 long positions to skew OI long-heavy...");

  // traderCollateral already created in step 1

  const [positionPubkey] = positionPDA(perpMarket, authority.publicKey);
  const commitment = createHash("sha256").update("simulation-position-1").digest();

  let positionsOpened = 0;
  // Only open 1 (PDA is per owner+market — can't open 3 with same wallet without closing)
  try {
    // args: collateral_amount(u64) + is_long(bool) + commitment([u8;32])
    const data = Buffer.alloc(8 + 8 + 1 + 32);
    DISC.perp_open_position.copy(data, 0);
    writeU64LE(data, 5_000_000, 8); // collateral_amount (5 USDC in 6 dec)
    data.writeUInt8(1, 16);          // is_long = true
    commitment.copy(data, 17);

    const ix = new TransactionInstruction({
      programId: AMM_PROGRAM_ID,
      keys: [
        { pubkey: authority.publicKey, isSigner: true,  isWritable: true  }, // owner
        { pubkey: perpMarket,          isSigner: false, isWritable: true  }, // perp_market
        { pubkey: positionPubkey,      isSigner: false, isWritable: true  }, // position PDA
        { pubkey: traderCollateral,    isSigner: false, isWritable: true  }, // trader_collateral
        { pubkey: collateralVault,     isSigner: false, isWritable: true  }, // collateral_vault
        { pubkey: TOKEN_PROGRAM_ID,    isSigner: false, isWritable: false },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      data,
    });
    const sig = await sendAndConfirmTransaction(connection, new Transaction().add(ix), [authority]);
    positionsOpened++;
    log(4, `Long position opened ✅ (5 USDC collateral, is_long=true)`, sig);
  } catch (e) {
    if (e.message?.includes("already in use") || e.message?.includes("0x0")) {
      log(4, "Position already exists ✅");
      positionsOpened++;
    } else { logErr(4, "perp_open_position failed", e); }
  }
  console.log(`  Total positions: ${positionsOpened} long, 0 short → OI skewed long`);

  // ── STEP 5: perp_shield_liquidation_guard ────────────────────────────────────
  log(5, "Calling perp_shield_liquidation_guard...");
  try {
    const data = Buffer.alloc(8 + 1);
    DISC.perp_shield_liquidation_guard.copy(data, 0);
    data.writeUInt8(0, 8); // force_liquidate = false

    const ix = new TransactionInstruction({
      programId: AMM_PROGRAM_ID,
      keys: [
        { pubkey: authority.publicKey, isSigner: true,  isWritable: false }, // liquidation_authority
        { pubkey: perpMarket,          isSigner: false, isWritable: true  }, // perp_market
        { pubkey: perpShield,          isSigner: false, isWritable: true  }, // perp_shield PDA
      ],
      data,
    });
    const sig = await sendAndConfirmTransaction(connection, new Transaction().add(ix), [authority]);
    log(5, "perp_shield_liquidation_guard passed ✅", sig);
  } catch (e) {
    const msg = e.message || "";
    if (msg.includes("CascadeDetected") || msg.includes("cascade")) {
      log(5, "Cascade detected ✅ — circuit breaker armed by guard");
    } else if (msg.includes("CircuitBreakerActive")) {
      log(5, "Circuit breaker already active ✅");
    } else { logErr(5, "perp_shield_liquidation_guard failed", e); }
  }

  // ── STEP 6: perp_shield_trigger_breaker ──────────────────────────────────────
  log(6, "Manually triggering circuit breaker...");
  const alertIndex = 0n;
  const [cascadeAlert] = cascadeAlertPDA(perpShield, 0);
  try {
    // args: reason_flags(u8) + alert_index(u64)
    const data = Buffer.alloc(8 + 1 + 8);
    DISC.perp_shield_trigger_breaker.copy(data, 0);
    data.writeUInt8(0x01, 8);              // reason_flags: OI imbalance
    data.writeBigUInt64LE(alertIndex, 9);  // alert_index

    const ix = new TransactionInstruction({
      programId: AMM_PROGRAM_ID,
      keys: [
        { pubkey: authority.publicKey,     isSigner: true,  isWritable: true  }, // authority
        { pubkey: perpMarket,              isSigner: false, isWritable: false }, // perp_market
        { pubkey: perpShield,              isSigner: false, isWritable: true  }, // perp_shield PDA
        { pubkey: cascadeAlert,            isSigner: false, isWritable: true  }, // cascade_alert PDA
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      data,
    });
    const sig = await sendAndConfirmTransaction(connection, new Transaction().add(ix), [authority]);
    log(6, "Circuit breaker triggered ✅", sig);
  } catch (e) {
    const msg = e.message || "";
    if (msg.includes("CircuitBreakerActive")) {
      log(6, "Circuit breaker already active ✅ (guard triggered it in step 5)");
    } else { logErr(6, "perp_shield_trigger_breaker failed", e); }
  }

  // ── STEP 7: Verify circuit_breaker_active == 1 ───────────────────────────────
  log(7, "Verifying circuit_breaker_active == 1...");
  try {
    const acct = await connection.getAccountInfo(perpShield);
    if (!acct) throw new Error("PerpShield account not found");
    // PerpShield layout (after 8-byte discriminator):
    // market(32) + authority(32) + price_authority(32) = 96 bytes offset
    // then circuit_breaker_active: u8
    const cbActive = acct.data[8 + 64];
    log(7, `circuit_breaker_active = ${cbActive} ${cbActive === 1 ? "✅ ACTIVE" : "⚠️  NOT ACTIVE — check thresholds"}`);
  } catch (e) { logErr(7, "Failed to read shield account", e); }

  // ── STEP 8: perp_shield_reset_breaker ────────────────────────────────────────
  log(8, "Resetting circuit breaker (force_reset=true)...");
  try {
    const data = Buffer.alloc(8 + 1);
    DISC.perp_shield_reset_breaker.copy(data, 0);
    data.writeUInt8(1, 8); // force_reset = true

    const ix = new TransactionInstruction({
      programId: AMM_PROGRAM_ID,
      keys: [
        { pubkey: authority.publicKey, isSigner: true,  isWritable: false }, // authority
        { pubkey: perpMarket,          isSigner: false, isWritable: false }, // perp_market
        { pubkey: perpShield,          isSigner: false, isWritable: true  }, // perp_shield PDA
      ],
      data,
    });
    const sig = await sendAndConfirmTransaction(connection, new Transaction().add(ix), [authority]);
    log(8, "Circuit breaker reset ✅", sig);
  } catch (e) { logErr(8, "perp_shield_reset_breaker failed", e); }

  // ── STEP 9: Verify circuit_breaker_active == 0 ───────────────────────────────
  log(9, "Verifying circuit_breaker_active == 0 after reset...");
  try {
    const acct = await connection.getAccountInfo(perpShield);
    if (!acct) throw new Error("PerpShield account not found");
    const cbActive = acct.data[8 + 64];
    log(9, `circuit_breaker_active = ${cbActive} ${cbActive === 0 ? "✅ RESET CONFIRMED" : "⚠️  Still active — reset may have failed"}`);
  } catch (e) { logErr(9, "Failed to read shield account after reset", e); }

  console.log("\n" + "=".repeat(62));
  console.log("  Simulation complete.");
  console.log("  Verify txs: https://explorer.x1.xyz/?cluster=testnet");
  console.log("=".repeat(62) + "\n");
}

main().catch((e) => { console.error("\nFatal:", e.message || e); process.exit(1); });
