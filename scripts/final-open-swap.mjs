/**
 * Nebula DEX — Open Position + Increase Liquidity + Swap
 * Pure @solana/web3.js + @solana/spl-token (no Anchor SDK, no Raydium SDK).
 *
 * Args order matches the actual on-chain Anchor signature in
 * programs/amm/src/lib.rs (NOT the inner v1 helper order):
 *   open_position(tick_lower, tick_upper, ta_lower_start, ta_upper_start,
 *                 liquidity, amount_0_max, amount_1_max)
 */
import {
  Connection, Keypair, PublicKey, Transaction,
  TransactionInstruction, SystemProgram, LAMPORTS_PER_SOL,
  SYSVAR_RENT_PUBKEY, ComputeBudgetProgram,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, NATIVE_MINT,
  getAssociatedTokenAddress, createSyncNativeInstruction,
  createAssociatedTokenAccountIdempotentInstruction,
} from "@solana/spl-token";
import { createHash } from "crypto";
import fs from "fs";

const RPC          = "https://rpc.testnet.x1.xyz";
const PROGRAM_ID   = new PublicKey("23dn1qvEfhPfBVvm46PGWMRRr3rjE7QSitPkzEEbeCtQ");
const KEYPAIR_PATH = process.env.NEBULA_KEYPAIR
  || "/root/.openclaw/workspace/memory/keys/NBLAsmKbxKW9cwJy7cfAhWMY9HJSwMj87qRWy6E3YGY.json";

const POOL_ID    = new PublicKey("E15UBYMftkvKfTGiuphngBUEfbJNvMs5Sit7aP6iRsCp");
const AMM_CONFIG = new PublicKey("25abQ5MB65HybBxnVjJ9urNxNAb7xDusBZTaedwKtiZ1");
const NUSDC_MINT = new PublicKey("XaPfbrpTsC1MiTyghZWg3bVbBYnz91DfdWTLVBtrUKe");
const TOKEN0     = NATIVE_MINT;
const TOKEN1     = NUSDC_MINT;
const METADATA_PROGRAM = new PublicKey("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

const TICK_ARRAY_SEED  = "tick_array";
const POSITION_SEED    = "position";
const POOL_VAULT_SEED  = "pool_vault";
const OBSERVATION_SEED = "observation";

const Q64 = 1n << 64n;
const U128_MAX = (1n << 128n) - 1n;
const MIN_SQRT_PRICE_X64 = 4295048016n;

// ── encoding helpers ─────────────────────────────────────────────────────────
const disc  = (n) => createHash("sha256").update(`global:${n}`).digest().slice(0, 8);
const i32LE = (n) => { const b = Buffer.alloc(4); b.writeInt32LE(n, 0); return b; };
const i32BE = (n) => { const b = Buffer.alloc(4); b.writeInt32BE(n, 0); return b; };
const u64LE = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n), 0); return b; };
const u128LE = (n) => {
  const b = Buffer.alloc(16);
  const v = BigInt(n);
  b.writeBigUInt64LE(v & 0xFFFFFFFFFFFFFFFFn, 0);
  b.writeBigUInt64LE(v >> 64n, 8);
  return b;
};
const boolByte = (v) => Buffer.from([v ? 1 : 0]);

// ── tick math (port of programs/amm/src/libraries/tick_math.rs) ─────────────
function getSqrtPriceAtTick(tick) {
  const absTick = BigInt(Math.abs(tick));
  let ratio = (absTick & 1n) !== 0n ? 0xfffcb933bd6fb800n : (1n << 64n);
  const factors = [
    0xfff97272373d4000n, // bit 1
    0xfff2e50f5f657000n,
    0xffe5caca7e10f000n,
    0xffcb9843d60f7000n,
    0xff973b41fa98e800n,
    0xff2ea16466c9b000n,
    0xfe5dee046a9a3800n,
    0xfcbe86c7900bb000n,
    0xf987a7253ac65800n,
    0xf3392b0822bb6000n,
    0xe7159475a2caf000n,
    0xd097f3bdfd2f2000n,
    0xa9f746462d9f8000n,
    0x70d869a156f31c00n,
    0x31be135f97ed3200n,
    0x9aa508b5b85a500n,
    0x5d6af8dedc582cn,
    0x2216e584f5fan,
  ];
  for (let i = 0; i < factors.length; i++) {
    if ((absTick & (1n << BigInt(i + 1))) !== 0n) {
      ratio = (ratio * factors[i]) >> 64n;
    }
  }
  if (tick > 0) ratio = U128_MAX / ratio;
  return ratio;
}

function getLiquidityFromAmount0(sqrtA, sqrtB, amount0) {
  if (sqrtA > sqrtB) [sqrtA, sqrtB] = [sqrtB, sqrtA];
  const intermediate = (sqrtA * sqrtB) / Q64;          // mul_div_floor(sqrtA, sqrtB, Q64)
  return (amount0 * intermediate) / (sqrtB - sqrtA);   // mul_div_floor
}
function getAmount0FromLiquidity(sqrtA, sqrtB, L) {
  if (sqrtA > sqrtB) [sqrtA, sqrtB] = [sqrtB, sqrtA];
  const num = L * (sqrtB - sqrtA) * Q64;
  const den = sqrtA * sqrtB;
  return (num + den - 1n) / den;  // ceil
}
function getAmount1FromLiquidity(sqrtA, sqrtB, L) {
  if (sqrtA > sqrtB) [sqrtA, sqrtB] = [sqrtB, sqrtA];
  const num = L * (sqrtB - sqrtA);
  return (num + Q64 - 1n) / Q64;  // ceil
}

function decodePoolBasics(raw) {
  let off = 8 + 1 + 32 + 32 + 32 + 32 + 32 + 32 + 32 + 1 + 1;
  const tickSpacing  = raw.readUInt16LE(off); off += 2;
  const liquidity    = raw.readBigUInt64LE(off) | (raw.readBigUInt64LE(off + 8) << 64n); off += 16;
  const sqrtPriceX64 = raw.readBigUInt64LE(off) | (raw.readBigUInt64LE(off + 8) << 64n); off += 16;
  const tickCurrent  = raw.readInt32LE(off);
  return { tickSpacing, liquidity, sqrtPriceX64, tickCurrent };
}

async function sendTx(conn, payer, ixs, signers = []) {
  const { blockhash, lastValidBlockHeight } = await conn.getLatestBlockhash();
  const tx = new Transaction({ recentBlockhash: blockhash, feePayer: payer.publicKey });
  tx.add(ComputeBudgetProgram.setComputeUnitLimit({ units: 600_000 }));
  for (const ix of ixs) tx.add(ix);
  tx.sign(payer, ...signers);
  const sig = await conn.sendRawTransaction(tx.serialize(), { skipPreflight: false });
  await conn.confirmTransaction({ signature: sig, blockhash, lastValidBlockHeight }, "confirmed");
  return sig;
}

async function main() {
  const conn = new Connection(RPC, "confirmed");
  const payer = Keypair.fromSecretKey(new Uint8Array(JSON.parse(fs.readFileSync(KEYPAIR_PATH, "utf-8"))));
  console.log("=== Nebula DEX — first real swap on X1 testnet ===");
  console.log("Payer:", payer.publicKey.toBase58());

  const poolAcc = await conn.getAccountInfo(POOL_ID);
  const pool = decodePoolBasics(poolAcc.data);
  console.log("tickSpacing:", pool.tickSpacing,
              "tick_current:", pool.tickCurrent,
              "sqrt_price_x64:", pool.sqrtPriceX64.toString(),
              "pool liquidity:", pool.liquidity.toString());

  // Each tick array covers TICK_ARRAY_SIZE(=60) * tick_spacing(=60) = 3600 ticks.
  // tick_current is -2161 → array start -3600. Position spans across current price.
  const TICK_LOWER = -3600;
  const TICK_UPPER = 3540;
  const TA_LOWER_START = -3600;
  const TA_UPPER_START = 0;
  if (TICK_LOWER % pool.tickSpacing !== 0 || TICK_UPPER % pool.tickSpacing !== 0)
    throw new Error("ticks not aligned to spacing");
  if (TA_LOWER_START % (60 * pool.tickSpacing) !== 0 || TA_UPPER_START % (60 * pool.tickSpacing) !== 0)
    throw new Error("tick array starts not aligned");
  console.log("Range:", TICK_LOWER, "→", TICK_UPPER,
              "| TA starts:", TA_LOWER_START, "/", TA_UPPER_START);

  const [vault0]      = PublicKey.findProgramAddressSync(
    [Buffer.from(POOL_VAULT_SEED), POOL_ID.toBuffer(), TOKEN0.toBuffer()], PROGRAM_ID);
  const [vault1]      = PublicKey.findProgramAddressSync(
    [Buffer.from(POOL_VAULT_SEED), POOL_ID.toBuffer(), TOKEN1.toBuffer()], PROGRAM_ID);
  const [observation] = PublicKey.findProgramAddressSync(
    [Buffer.from(OBSERVATION_SEED), POOL_ID.toBuffer()], PROGRAM_ID);
  const [tickArrayLower] = PublicKey.findProgramAddressSync(
    [Buffer.from(TICK_ARRAY_SEED), POOL_ID.toBuffer(), i32BE(TA_LOWER_START)], PROGRAM_ID);
  const [tickArrayUpper] = PublicKey.findProgramAddressSync(
    [Buffer.from(TICK_ARRAY_SEED), POOL_ID.toBuffer(), i32BE(TA_UPPER_START)], PROGRAM_ID);
  console.log("vault0:", vault0.toBase58());
  console.log("vault1:", vault1.toBase58());
  console.log("observation:", observation.toBase58());
  console.log("tick_array_lower:", tickArrayLower.toBase58());
  console.log("tick_array_upper:", tickArrayUpper.toBase58());

  const nftMint = Keypair.generate();
  const [personalPosition] = PublicKey.findProgramAddressSync(
    [Buffer.from(POSITION_SEED), nftMint.publicKey.toBuffer()], PROGRAM_ID);
  const nftAta = await getAssociatedTokenAddress(nftMint.publicKey, payer.publicKey);
  const [metadataAccount] = PublicKey.findProgramAddressSync(
    [Buffer.from("metadata"), METADATA_PROGRAM.toBuffer(), nftMint.publicKey.toBuffer()],
    METADATA_PROGRAM);
  console.log("nftMint:", nftMint.publicKey.toBase58());
  console.log("personalPosition:", personalPosition.toBase58());

  const wxntAta  = await getAssociatedTokenAddress(TOKEN0, payer.publicKey);
  const nusdcAta = await getAssociatedTokenAddress(TOKEN1, payer.publicKey);
  console.log("wxntAta:", wxntAta.toBase58());
  console.log("nusdcAta:", nusdcAta.toBase58());

  // ── Step 1: wrap 5 XNT into WXNT ─────────────────────────────────────────
  console.log("\n[1] Wrap 5 XNT into WXNT");
  const wrapIxs = [
    createAssociatedTokenAccountIdempotentInstruction(payer.publicKey, wxntAta, payer.publicKey, TOKEN0),
    SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: wxntAta, lamports: 5_000_000_000n }),
    createSyncNativeInstruction(wxntAta),
  ];
  console.log("  tx:", await sendTx(conn, payer, wrapIxs));
  const wxntBal = await conn.getTokenAccountBalance(wxntAta);
  console.log("  WXNT balance:", wxntBal.value.uiAmountString);

  // ── Step 2: open_position (liquidity=0; bootstraps tick arrays + NFT + position) ──
  const protocolPosition = SystemProgram.programId; // deprecated sentinel

  // ARGS in declared order (lib.rs::open_position):
  //   tick_lower, tick_upper, ta_lower_start, ta_upper_start, liquidity, amount_0_max, amount_1_max
  const openData = Buffer.concat([
    disc("open_position"),
    i32LE(TICK_LOWER),
    i32LE(TICK_UPPER),
    i32LE(TA_LOWER_START),
    i32LE(TA_UPPER_START),
    u128LE(0n),                  // liquidity=0 → add_liquidity short-circuits (base_flag is None in v1)
    u64LE(5_000_000_000n),       // amount_0_max (unused when liquidity=0)
    u64LE(1_000_000_000_000n),   // amount_1_max
  ]);

  const openIx = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      { pubkey: payer.publicKey,             isSigner: true,  isWritable: true  }, //  0 payer
      { pubkey: payer.publicKey,             isSigner: false, isWritable: false }, //  1 position_nft_owner
      { pubkey: nftMint.publicKey,           isSigner: true,  isWritable: true  }, //  2 position_nft_mint
      { pubkey: nftAta,                      isSigner: false, isWritable: true  }, //  3 position_nft_account
      { pubkey: metadataAccount,             isSigner: false, isWritable: true  }, //  4 metadata_account
      { pubkey: POOL_ID,                     isSigner: false, isWritable: true  }, //  5 pool_state
      { pubkey: protocolPosition,            isSigner: false, isWritable: false }, //  6 protocol_position
      { pubkey: tickArrayLower,              isSigner: false, isWritable: true  }, //  7 tick_array_lower
      { pubkey: tickArrayUpper,              isSigner: false, isWritable: true  }, //  8 tick_array_upper
      { pubkey: personalPosition,            isSigner: false, isWritable: true  }, //  9 personal_position
      { pubkey: wxntAta,                     isSigner: false, isWritable: true  }, // 10 token_account_0
      { pubkey: nusdcAta,                    isSigner: false, isWritable: true  }, // 11 token_account_1
      { pubkey: vault0,                      isSigner: false, isWritable: true  }, // 12 token_vault_0
      { pubkey: vault1,                      isSigner: false, isWritable: true  }, // 13 token_vault_1
      { pubkey: SYSVAR_RENT_PUBKEY,          isSigner: false, isWritable: false }, // 14 rent
      { pubkey: SystemProgram.programId,     isSigner: false, isWritable: false }, // 15 system_program
      { pubkey: TOKEN_PROGRAM_ID,            isSigner: false, isWritable: false }, // 16 token_program
      { pubkey: ASSOCIATED_TOKEN_PROGRAM_ID, isSigner: false, isWritable: false }, // 17 associated_token_program
      { pubkey: METADATA_PROGRAM,            isSigner: false, isWritable: false }, // 18 metadata_program
    ],
    data: openData,
  });

  console.log("\n[2] open_position (liquidity=0 — creates position + tick arrays + NFT + metadata)");
  try {
    const sig = await sendTx(conn, payer, [openIx], [nftMint]);
    console.log("  tx:", sig);
  } catch (e) {
    console.error("  open_position failed:", e.message);
    if (e.logs) console.error("  logs:\n" + e.logs.slice(-25).join("\n"));
    throw e;
  }

  // ── Step 3: compute L and increase_liquidity ────────────────────────────
  const sqrtP        = pool.sqrtPriceX64;
  const sqrtP_lower  = getSqrtPriceAtTick(TICK_LOWER);
  const sqrtP_upper  = getSqrtPriceAtTick(TICK_UPPER);
  const depositAmount0 = 3_500_000_000n; // 3.5 SOL
  const liquidity = getLiquidityFromAmount0(sqrtP, sqrtP_upper, depositAmount0);
  const need0 = getAmount0FromLiquidity(sqrtP, sqrtP_upper, liquidity);
  const need1 = getAmount1FromLiquidity(sqrtP_lower, sqrtP, liquidity);
  console.log("\n[3] increase_liquidity");
  console.log("  L =", liquidity.toString());
  console.log("  expected token0 needed (lamports):", need0.toString(),
              "(=", Number(need0) / 1e9, "SOL)");
  console.log("  expected token1 needed (NUSDC raw):", need1.toString(),
              "(=", Number(need1) / 1e6, "NUSDC)");

  // increase_liquidity v1 args: liquidity, amount_0_max, amount_1_max
  const incData = Buffer.concat([
    disc("increase_liquidity"),
    u128LE(liquidity),
    u64LE(need0 + 50_000_000n),     // 0.05 SOL slack
    u64LE(need1 + 100_000_000n),    // 100 NUSDC slack
  ]);

  // increase_liquidity v1 accounts (see IncreaseLiquidity struct):
  //  nft_owner, nft_account, pool_state, protocol_position, personal_position,
  //  tick_array_lower, tick_array_upper, token_account_0, token_account_1,
  //  token_vault_0, token_vault_1, token_program
  const incIx = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      { pubkey: payer.publicKey,   isSigner: true,  isWritable: false },
      { pubkey: nftAta,            isSigner: false, isWritable: false },
      { pubkey: POOL_ID,           isSigner: false, isWritable: true  },
      { pubkey: protocolPosition,  isSigner: false, isWritable: false },
      { pubkey: personalPosition,  isSigner: false, isWritable: true  },
      { pubkey: tickArrayLower,    isSigner: false, isWritable: true  },
      { pubkey: tickArrayUpper,    isSigner: false, isWritable: true  },
      { pubkey: wxntAta,           isSigner: false, isWritable: true  },
      { pubkey: nusdcAta,          isSigner: false, isWritable: true  },
      { pubkey: vault0,            isSigner: false, isWritable: true  },
      { pubkey: vault1,            isSigner: false, isWritable: true  },
      { pubkey: TOKEN_PROGRAM_ID,  isSigner: false, isWritable: false },
    ],
    data: incData,
  });
  try {
    const sig = await sendTx(conn, payer, [incIx]);
    console.log("  tx:", sig);
  } catch (e) {
    console.error("  increase_liquidity failed:", e.message);
    if (e.logs) console.error("  logs:\n" + e.logs.slice(-25).join("\n"));
    throw e;
  }

  // ── Step 4: swap 0.5 XNT → NUSDC ─────────────────────────────────────────
  console.log("\n[4] swap 0.5 XNT → NUSDC");
  const swapData = Buffer.concat([
    disc("swap"),
    u64LE(500_000_000n),              // amount_in = 0.5 XNT
    u64LE(1n),                        // min out
    u128LE(MIN_SQRT_PRICE_X64 + 1n),  // sqrt_price_limit (zero_for_one)
    boolByte(true),                   // is_base_input
  ]);

  // swap (v1) accounts: payer, amm_config, pool_state, input_token_account, output_token_account,
  //   input_vault, output_vault, observation_state, token_program, tick_array
  // remaining: extra tick arrays for crossings
  const swapIx = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      { pubkey: payer.publicKey,  isSigner: true,  isWritable: false },
      { pubkey: AMM_CONFIG,       isSigner: false, isWritable: false },
      { pubkey: POOL_ID,          isSigner: false, isWritable: true  },
      { pubkey: wxntAta,          isSigner: false, isWritable: true  },
      { pubkey: nusdcAta,         isSigner: false, isWritable: true  },
      { pubkey: vault0,           isSigner: false, isWritable: true  },
      { pubkey: vault1,           isSigner: false, isWritable: true  },
      { pubkey: observation,      isSigner: false, isWritable: true  },
      { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
      { pubkey: tickArrayLower,   isSigner: false, isWritable: true  }, // current tick array
      { pubkey: tickArrayUpper,   isSigner: false, isWritable: true  }, // remaining (in case of crossings)
    ],
    data: swapData,
  });

  const beforeNusdc = await conn.getTokenAccountBalance(nusdcAta);
  const beforeWxnt  = await conn.getTokenAccountBalance(wxntAta);
  let swapSig;
  try {
    swapSig = await sendTx(conn, payer, [swapIx]);
    console.log("  tx:", swapSig);
  } catch (e) {
    console.error("  swap failed:", e.message);
    if (e.logs) console.error("  logs:\n" + e.logs.slice(-25).join("\n"));
    throw e;
  }
  const afterNusdc = await conn.getTokenAccountBalance(nusdcAta);
  const afterWxnt  = await conn.getTokenAccountBalance(wxntAta);
  console.log("\n=== Final balances ===");
  console.log("WXNT :", afterWxnt.value.uiAmountString,
              " (Δ", (Number(afterWxnt.value.uiAmount) - Number(beforeWxnt.value.uiAmount)).toFixed(6), ")");
  console.log("NUSDC:", afterNusdc.value.uiAmountString,
              " (Δ +", (Number(afterNusdc.value.uiAmount) - Number(beforeNusdc.value.uiAmount)).toFixed(6), ")");
  console.log("\nDONE — swap confirmed at", swapSig);
}

main().catch((e) => { console.error(e); process.exit(1); });
