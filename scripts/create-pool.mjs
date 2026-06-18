/**
 * Nebula DEX — Create Test Pool + Swap
 * Creates XNT/USDC test pool and executes a real swap on X1 testnet
 */

import {
  Connection, Keypair, PublicKey, Transaction,
  TransactionInstruction, SystemProgram, LAMPORTS_PER_SOL,
  SYSVAR_RENT_PUBKEY
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID,
  createMint, mintTo, getOrCreateAssociatedTokenAccount,
  createInitializeMintInstruction, getMinimumBalanceForRentExemptMint,
  MINT_SIZE
} from "@solana/spl-token";
import { createHash } from "crypto";
import fs from "fs";
import BN from "bn.js";

const RPC = "https://rpc.testnet.x1.xyz";
const PROGRAM_ID = new PublicKey("23dn1qvEfhPfBVvm46PGWMRRr3rjE7QSitPkzEEbeCtQ");
const KEYPAIR_PATH = "/root/.openclaw/workspace/memory/keys/NBLAsmKbxKW9cwJy7cfAhWMY9HJSwMj87qRWy6E3YGY.json";

// Wrapped XNT (native token on X1)
const WNXNT = new PublicKey("So11111111111111111111111111111111111111112");

function discriminator(name) {
  return createHash("sha256").update(`global:${name}`).digest().slice(0, 8);
}

function u16BE(n) { const b = Buffer.alloc(2); b.writeUInt16BE(n, 0); return b; }
function u64LE(n) { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n), 0); return b; }
function u128LE(n) {
  const b = Buffer.alloc(16);
  const big = BigInt(n);
  b.writeBigUInt64LE(big & 0xFFFFFFFFFFFFFFFFn, 0);
  b.writeBigUInt64LE(big >> 64n, 8);
  return b;
}

async function main() {
  const connection = new Connection(RPC, "confirmed");
  const kpData = JSON.parse(fs.readFileSync(KEYPAIR_PATH, "utf-8"));
  const payer = Keypair.fromSecretKey(new Uint8Array(kpData));

  console.log("=== Nebula DEX — Create Pool & Swap ===");
  console.log("Admin:", payer.publicKey.toBase58());
  const bal = await connection.getBalance(payer.publicKey);
  console.log("Balance:", (bal / LAMPORTS_PER_SOL).toFixed(4), "XNT\n");

  // ── Step 1: Create test USDC mint ──────────────────────────────────────────
  console.log("Step 1: Creating test NUSDC token mint...");
  const usdcMint = await createMint(
    connection, payer, payer.publicKey, payer.publicKey, 6
  );
  console.log("  NUSDC Mint:", usdcMint.toBase58());

  // ── Step 2: Create token accounts + mint test tokens ─────────────────────
  console.log("\nStep 2: Creating token accounts...");
  const payerUsdc = await getOrCreateAssociatedTokenAccount(
    connection, payer, usdcMint, payer.publicKey
  );
  console.log("  Payer NUSDC ATA:", payerUsdc.address.toBase58());

  // Mint 1,000,000 NUSDC to payer
  await mintTo(connection, payer, usdcMint, payerUsdc.address, payer, 1_000_000_000_000n);
  console.log("  Minted 1,000,000 NUSDC");

  // ── Step 3: Derive pool PDA ───────────────────────────────────────────────
  console.log("\nStep 3: Deriving pool address...");

  // AMM config for 0.25% (index 2)
  const [ammConfig] = PublicKey.findProgramAddressSync(
    [Buffer.from("amm_config"), u16BE(2)],
    PROGRAM_ID
  );
  console.log("  AMM Config (0.25%):", ammConfig.toBase58());

  // Sort mints — token0 < token1 by pubkey
  let token0Mint, token1Mint;
  if (WNXNT.toBuffer().compare(usdcMint.toBuffer()) < 0) {
    token0Mint = WNXNT;
    token1Mint = usdcMint;
  } else {
    token0Mint = usdcMint;
    token1Mint = WNXNT;
  }
  console.log("  Token0:", token0Mint.toBase58());
  console.log("  Token1:", token1Mint.toBase58());

  // Pool PDA: seeds = ["pool", amm_config, token0_mint, token1_mint]
  const [poolState] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool"), ammConfig.toBuffer(), token0Mint.toBuffer(), token1Mint.toBuffer()],
    PROGRAM_ID
  );
  console.log("  Pool PDA:", poolState.toBase58());

  // ── Step 4: Create pool vaults and observation account ───────────────────
  const [token0Vault] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool_vault"), poolState.toBuffer(), token0Mint.toBuffer()],
    PROGRAM_ID
  );
  const [token1Vault] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool_vault"), poolState.toBuffer(), token1Mint.toBuffer()],
    PROGRAM_ID
  );
  const [observationState] = PublicKey.findProgramAddressSync(
    [Buffer.from("observation"), poolState.toBuffer()],
    PROGRAM_ID
  );
  const [tickArrayBitmap] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool_tick_array_bitmap_extension"), poolState.toBuffer()],
    PROGRAM_ID
  );

  console.log("  Token0 Vault:", token0Vault.toBase58());
  console.log("  Token1 Vault:", token1Vault.toBase58());

  // ── Step 5: Create pool ───────────────────────────────────────────────────
  console.log("\nStep 4: Creating pool...");

  // Initial price: 1 XNT = 0.80 USDC
  // sqrt_price_x64 = sqrt(0.80) * 2^64 = 0.8944 * 18446744073709551616
  // sqrt(0.80) * 2^64 — price of 1 token0 in token1 terms
  const sqrtPrice = BigInt("16558302939472547022"); // sqrt(0.80) * 2^64
  const openTime = BigInt(0); // 0 = open immediately (must be < block_timestamp)

  const createPoolDisc = discriminator("create_pool");
  const createPoolData = Buffer.concat([
    createPoolDisc,
    u128LE(sqrtPrice),
    u64LE(openTime),
  ]);

  const createPoolIx = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      { pubkey: payer.publicKey, isSigner: true, isWritable: true },
      { pubkey: ammConfig, isSigner: false, isWritable: false },
      { pubkey: poolState, isSigner: false, isWritable: true },
      { pubkey: token0Mint, isSigner: false, isWritable: false },
      { pubkey: token1Mint, isSigner: false, isWritable: false },
      { pubkey: token0Vault, isSigner: false, isWritable: true },
      { pubkey: token1Vault, isSigner: false, isWritable: true },
      { pubkey: observationState, isSigner: false, isWritable: true },
      { pubkey: tickArrayBitmap, isSigner: false, isWritable: true },
      { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
      { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      { pubkey: SYSVAR_RENT_PUBKEY, isSigner: false, isWritable: false },
    ],
    data: createPoolData,
  });

  try {
    const { blockhash } = await connection.getLatestBlockhash();
    const tx = new Transaction({ recentBlockhash: blockhash, feePayer: payer.publicKey });
    tx.add(createPoolIx);
    tx.sign(payer);
    const sig = await connection.sendRawTransaction(tx.serialize(), { skipPreflight: false });
    await connection.confirmTransaction(sig, "confirmed");
    console.log("  ✅ Pool created!");
    console.log("  Pool:", poolState.toBase58());
    console.log("  Tx:", sig);
  } catch (err) {
    console.error("  ❌ Create pool failed:", err.message);
    if (err.logs) console.error("  Logs:\n", err.logs.slice(-8).join("\n"));
    process.exit(1);
  }

  // ── Done ──────────────────────────────────────────────────────────────────
  const finalBal = await connection.getBalance(payer.publicKey);
  console.log("\n=== Pool creation complete ===");
  console.log("Pool address:", poolState.toBase58());
  console.log("NUSDC mint:", usdcMint.toBase58());
  console.log("Final balance:", (finalBal / LAMPORTS_PER_SOL).toFixed(4), "XNT");
  console.log("\nNext: add liquidity, then test swap");
}

main().catch(console.error);
