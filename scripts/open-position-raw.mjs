/**
 * Nebula DEX — Raw open_position + increase_liquidity + swap
 * No SDK. Pure @solana/web3.js only.
 */

import {
  Connection, Keypair, PublicKey, Transaction,
  TransactionInstruction, SystemProgram, LAMPORTS_PER_SOL,
  SYSVAR_RENT_PUBKEY
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, NATIVE_MINT,
  getOrCreateAssociatedTokenAccount, createSyncNativeInstruction,
  createAssociatedTokenAccountInstruction
} from "@solana/spl-token";
import { createHash } from "crypto";
import fs from "fs";

const RPC        = "https://rpc.testnet.x1.xyz";
const PROGRAM_ID = new PublicKey("23dn1qvEfhPfBVvm46PGWMRRr3rjE7QSitPkzEEbeCtQ");
const KEYPAIR_PATH = "/root/.openclaw/workspace/memory/keys/NBLAsmKbxKW9cwJy7cfAhWMY9HJSwMj87qRWy6E3YGY.json";

// Existing pool state from create-pool step
const POOL_ID    = new PublicKey("DLisUiGfJR7Gmrv2QjUGGgQYHz97bsj7HqgdSFUJjNU1");
const NUSDC_MINT = new PublicKey("Gax1LXZK1GXwHKm2pWuZ4eKxeR7YPpA9DV64FdPofMFo");
const TOKEN0     = NATIVE_MINT; // WXNT = So111...112
const TOKEN1     = NUSDC_MINT;

// Metaplex metadata program (mainnet/testnet same address)
const METADATA_PROGRAM = new PublicKey("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

// Seeds
const TICK_ARRAY_SEED   = "tick_array";
const POSITION_SEED     = "position";
const POOL_BITMAP_SEED  = "pool_tick_array_bitmap_extension";

function disc(name) {
  return createHash("sha256").update(`global:${name}`).digest().slice(0, 8);
}
function i32BE(n) { const b=Buffer.alloc(4); b.writeInt32BE(n,0); return b; }
function i32LE(n) { const b=Buffer.alloc(4); b.writeInt32LE(n,0); return b; }
function u64LE(n) { const b=Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n),0); return b; }
function u128LE(n) {
  const b=Buffer.alloc(16);
  const v=BigInt(n);
  b.writeBigUInt64LE(v & 0xFFFFFFFFFFFFFFFFn,0);
  b.writeBigUInt64LE(v >> 64n,8);
  return b;
}
function bool1(v) { return Buffer.from([v?1:0]); }
function optBool(v) {  // Rust Option<bool>: None=0x00, Some(false)=0x01,0x00, Some(true)=0x01,0x01
  if (v === null || v === undefined) return Buffer.from([0]);
  return Buffer.from([1, v?1:0]);
}

async function sendTx(conn, payer, ixs, extraSigners=[]) {
  const { blockhash, lastValidBlockHeight } = await conn.getLatestBlockhash();
  const tx = new Transaction({ recentBlockhash: blockhash, feePayer: payer.publicKey });
  for (const ix of ixs) tx.add(ix);
  tx.sign(payer, ...extraSigners);
  const sig = await conn.sendRawTransaction(tx.serialize(), { skipPreflight: false });
  await conn.confirmTransaction({ signature: sig, blockhash, lastValidBlockHeight }, "confirmed");
  return sig;
}

async function main() {
  const conn = new Connection(RPC, "confirmed");
  const kp = JSON.parse(fs.readFileSync(KEYPAIR_PATH,"utf-8"));
  const payer = Keypair.fromSecretKey(new Uint8Array(kp));

  console.log("=== Nebula DEX — Raw Open Position & Swap ===");
  console.log("Pool:", POOL_ID.toBase58());
  const bal = await conn.getBalance(payer.publicKey);
  console.log("Balance:", (bal/LAMPORTS_PER_SOL).toFixed(4), "XNT\n");

  // ── Derive pool vaults ──────────────────────────────────────────────────────
  const [vault0] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool_vault"), POOL_ID.toBuffer(), TOKEN0.toBuffer()], PROGRAM_ID);
  const [vault1] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool_vault"), POOL_ID.toBuffer(), TOKEN1.toBuffer()], PROGRAM_ID);
  const [observation] = PublicKey.findProgramAddressSync(
    [Buffer.from("observation"), POOL_ID.toBuffer()], PROGRAM_ID);
  const [bitmap] = PublicKey.findProgramAddressSync(
    [Buffer.from(POOL_BITMAP_SEED), POOL_ID.toBuffer()], PROGRAM_ID);

  console.log("Vault0:", vault0.toBase58());
  console.log("Vault1:", vault1.toBase58());

  // ── Tick range around current price ─────────────────────────────────────────
  // Current price ~0.80, current tick ~-2231
  // TICK_ARRAY_SIZE=60, tickSpacing=60 → each array covers 3600 ticks
  // Position: -3540 to 3540 (spans two tick arrays: -3600 to 0)
  const TICK_LOWER = -3540; // lower tick, brackets tickCurrent=-2161, array start=-3600
  const TICK_UPPER = -60;   // upper tick, brackets tickCurrent=-2161, array start=-3600
  const taLowerStart = -3600; // array containing tick -3540 and -60
  const taUpperStart = -3600; // same array as lower
  console.log("Tick range:", TICK_LOWER, "to", TICK_UPPER, "(around current price 0.80)");
  console.log("TickArray starts:", taLowerStart, "/", taUpperStart);

  const [tickArrayLower] = PublicKey.findProgramAddressSync(
    [Buffer.from(TICK_ARRAY_SEED), POOL_ID.toBuffer(), i32BE(taLowerStart)], PROGRAM_ID);
  const [tickArrayUpper] = PublicKey.findProgramAddressSync(
    [Buffer.from(TICK_ARRAY_SEED), POOL_ID.toBuffer(), i32BE(taUpperStart)], PROGRAM_ID);

  console.log("TickArray Lower:", tickArrayLower.toBase58());
  console.log("TickArray Upper:", tickArrayUpper.toBase58());

  // ── Generate position NFT mint keypair ─────────────────────────────────────
  const nftMint = Keypair.generate();
  console.log("\nPosition NFT Mint:", nftMint.publicKey.toBase58());

  // ── Derive PDAs ─────────────────────────────────────────────────────────────
  const [personalPosition] = PublicKey.findProgramAddressSync(
    [Buffer.from(POSITION_SEED), nftMint.publicKey.toBuffer()], PROGRAM_ID);

  // NFT token account (ATA of payer for the nft mint)
  const [nftTokenAccount] = PublicKey.findProgramAddressSync(
    [payer.publicKey.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), nftMint.publicKey.toBuffer()],
    ASSOCIATED_TOKEN_PROGRAM_ID
  );

  // Metaplex metadata PDA (even with with_metadata=false, account must be in list)
  const [metadataAccount] = PublicKey.findProgramAddressSync(
    [Buffer.from("metadata"), METADATA_PROGRAM.toBuffer(), nftMint.publicKey.toBuffer()],
    METADATA_PROGRAM
  );

  // protocol_position = deprecated, pass SystemProgram
  const protocolPosition = SystemProgram.programId;

  console.log("Personal Position:", personalPosition.toBase58());
  console.log("NFT Token Account:", nftTokenAccount.toBase58());

  // ── Payer token accounts ────────────────────────────────────────────────────
  // WXNT ATA (wrap SOL)
  const wxntAta = await getOrCreateAssociatedTokenAccount(conn, payer, TOKEN0, payer.publicKey);
  const nusdcAta = await getOrCreateAssociatedTokenAccount(conn, payer, TOKEN1, payer.publicKey);

  // Wrap 10 XNT into WXNT
  console.log("\nWrapping 10 XNT → WXNT...");
  const wrapIxs = [
    SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: wxntAta.address, lamports: 10_000_000_000n }),
    createSyncNativeInstruction(wxntAta.address),
  ];
  const wrapSig = await sendTx(conn, payer, wrapIxs);
  console.log("Wrapped:", wrapSig);

  // ── Build open_position instruction ────────────────────────────────────────
  // Instruction: open_position(liquidity, amount_0_max, amount_1_max, tick_lower, tick_upper,
  //               tick_array_lower_start, tick_array_upper_start, with_metadata, base_flag)
  // disc = discriminator("open_position")
  // Args encoding: u128 + u64 + u64 + i32 + i32 + i32 + i32 + bool + Option<bool>

  const LIQUIDITY = 10_000_000_000n; // 10B liquidity units
  const AMOUNT_0_MAX = 5_000_000_000n; // 5 XNT max
  const AMOUNT_1_MAX = 4_000_000_000n; // 4 NUSDC max (6 decimals)

  const openPosData = Buffer.concat([
    disc("open_position"),
    i32LE(TICK_LOWER),
    i32LE(TICK_UPPER),
    i32LE(taLowerStart),
    i32LE(taUpperStart),
    u128LE(LIQUIDITY),
    u64LE(AMOUNT_0_MAX),
    u64LE(AMOUNT_1_MAX),
    bool1(false),
    optBool(null),
  ]);

  console.log("\nOpening position...");
  console.log("Instruction data:", openPosData.toString("hex"));

  const openPosIx = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      { pubkey: payer.publicKey,             isSigner: true,  isWritable: true  }, // payer
      { pubkey: payer.publicKey,             isSigner: false, isWritable: false }, // position_nft_owner
      { pubkey: nftMint.publicKey,           isSigner: true,  isWritable: true  }, // position_nft_mint
      { pubkey: nftTokenAccount,             isSigner: false, isWritable: true  }, // position_nft_account
      { pubkey: metadataAccount,             isSigner: false, isWritable: true  }, // metadata_account
      { pubkey: POOL_ID,                     isSigner: false, isWritable: true  }, // pool_state
      { pubkey: protocolPosition,            isSigner: false, isWritable: false }, // protocol_position (deprecated)
      { pubkey: tickArrayLower,              isSigner: false, isWritable: true  }, // tick_array_lower
      { pubkey: tickArrayUpper,              isSigner: false, isWritable: true  }, // tick_array_upper
      { pubkey: personalPosition,            isSigner: false, isWritable: true  }, // personal_position
      { pubkey: wxntAta.address,             isSigner: false, isWritable: true  }, // token_account_0
      { pubkey: nusdcAta.address,            isSigner: false, isWritable: true  }, // token_account_1
      { pubkey: vault0,                      isSigner: false, isWritable: true  }, // token_vault_0
      { pubkey: vault1,                      isSigner: false, isWritable: true  }, // token_vault_1
      { pubkey: SYSVAR_RENT_PUBKEY,          isSigner: false, isWritable: false }, // rent
      { pubkey: SystemProgram.programId,     isSigner: false, isWritable: false }, // system_program
      { pubkey: TOKEN_PROGRAM_ID,            isSigner: false, isWritable: false }, // token_program
      { pubkey: ASSOCIATED_TOKEN_PROGRAM_ID, isSigner: false, isWritable: false }, // associated_token_program
      { pubkey: METADATA_PROGRAM,            isSigner: false, isWritable: false }, // metadata_program
    ],
    data: openPosData,
  });

  try {
    const sig = await sendTx(conn, payer, [openPosIx], [nftMint]);
    console.log("✅ Position opened!");
    console.log("Personal position:", personalPosition.toBase58());
    console.log("Tx:", sig);
  } catch (err) {
    console.error("❌ open_position failed:", err.message);
    if (err.logs) console.error("Last logs:\n", err.logs.slice(-10).join("\n"));
    process.exit(1);
  }

  // ── Swap: 1 XNT → NUSDC ────────────────────────────────────────────────────
  console.log("\n=== Test Swap: 1 XNT → NUSDC ===");

  // swap_v2: amount, other_amount_threshold, sqrt_price_limit_x64, is_base_input, zero_for_one
  // disc = discriminator("swap_v2")
  // zero_for_one = true (token0→token1 = WXNT→NUSDC)
  const SQRT_PRICE_MIN = 4295048016n; // min sqrt price
  const swapData = Buffer.concat([
    disc("swap_v2"),
    u64LE(1_000_000_000n),  // amount = 1 XNT
    u64LE(1n),              // other_amount_threshold = 1 (any output)
    u128LE(SQRT_PRICE_MIN), // sqrt_price_limit_x64 = min (selling token0)
    bool1(true),            // is_base_input = true
    bool1(true),            // zero_for_one = true
  ]);

  // swap_v2 accounts: payer, amm_config, pool_state, input_token_account,
  //   output_token_account, input_vault, output_vault, observation_state,
  //   token_program, token_program_2022, memo_program, input_vault_mint, output_vault_mint
  // remaining_accounts: tick arrays covering current price

  // Get current tick to find tick array
  const poolInfo = await conn.getAccountInfo(POOL_ID);
  // current tick is at offset 8+1+8+32+32+32+32+32+1+2 = 180 (approx) — skip for now, use tick 0 array
  const [tickArrayCurrent] = PublicKey.findProgramAddressSync(
    [Buffer.from(TICK_ARRAY_SEED), POOL_ID.toBuffer(), i32BE(0)], PROGRAM_ID);

  const MEMO_PROGRAM = new PublicKey("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

  const swapIx = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      { pubkey: payer.publicKey,         isSigner: true,  isWritable: false }, // payer
      { pubkey: new PublicKey("25abQ5MB65HybBxnVjJ9urNxNAb7xDusBZTaedwKtiZ1"), isSigner: false, isWritable: false }, // amm_config
      { pubkey: POOL_ID,                 isSigner: false, isWritable: true  }, // pool_state
      { pubkey: wxntAta.address,         isSigner: false, isWritable: true  }, // input_token_account
      { pubkey: nusdcAta.address,        isSigner: false, isWritable: true  }, // output_token_account
      { pubkey: vault0,                  isSigner: false, isWritable: true  }, // input_vault (token0)
      { pubkey: vault1,                  isSigner: false, isWritable: true  }, // output_vault (token1)
      { pubkey: observation,             isSigner: false, isWritable: true  }, // observation_state
      { pubkey: TOKEN_PROGRAM_ID,        isSigner: false, isWritable: false }, // token_program
      { pubkey: TOKEN_PROGRAM_ID,        isSigner: false, isWritable: false }, // token_program_2022 (use same)
      { pubkey: MEMO_PROGRAM,            isSigner: false, isWritable: false }, // memo_program
      { pubkey: TOKEN0,                  isSigner: false, isWritable: false }, // input_vault_mint
      { pubkey: TOKEN1,                  isSigner: false, isWritable: false }, // output_vault_mint
    ],
    data: swapData,
  });
  // Pass tick array covering tick 0 as remaining account
  swapIx.keys.push({ pubkey: tickArrayCurrent, isSigner: false, isWritable: true });

  try {
    const sig = await sendTx(conn, payer, [swapIx]);
    console.log("✅ SWAP SUCCESSFUL!");
    console.log("Tx:", sig);
    // Check output balance
    const nusdcBal = await conn.getTokenAccountBalance(nusdcAta.address);
    console.log("NUSDC balance after swap:", nusdcBal.value.uiAmountString);
  } catch (err) {
    console.error("❌ Swap failed:", err.message);
    if (err.logs) console.error("Logs:\n", err.logs.slice(-10).join("\n"));
  }

  const finalBal = await conn.getBalance(payer.publicKey);
  console.log("\nFinal XNT balance:", (finalBal/LAMPORTS_PER_SOL).toFixed(4));
}

main().catch(console.error);
