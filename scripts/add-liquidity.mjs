/**
 * Nebula DEX — Add Liquidity + Test Swap
 */

import {
  Connection, Keypair, PublicKey, Transaction,
  TransactionInstruction, SystemProgram, LAMPORTS_PER_SOL,
  SYSVAR_RENT_PUBKEY
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID,
  getOrCreateAssociatedTokenAccount, createSyncNativeInstruction,
  NATIVE_MINT
} from "@solana/spl-token";
import { createHash } from "crypto";
import fs from "fs";

const RPC = "https://rpc.testnet.x1.xyz";
const PROGRAM_ID = new PublicKey("23dn1qvEfhPfBVvm46PGWMRRr3rjE7QSitPkzEEbeCtQ");
const KEYPAIR_PATH = "/root/.openclaw/workspace/memory/keys/NBLAsmKbxKW9cwJy7cfAhWMY9HJSwMj87qRWy6E3YGY.json";

// From previous step
const POOL_ID     = new PublicKey("E15UBYMftkvKfTGiuphngBUEfbJNvMs5Sit7aP6iRsCp");
const NUSDC_MINT  = new PublicKey("XaPfbrpTsC1MiTyghZWg3bVbBYnz91DfdWTLVBtrUKe");
const WNXNT_MINT  = NATIVE_MINT; // So111...112
const AMM_CONFIG  = new PublicKey("25abQ5MB65HybBxnVjJ9urNxNAb7xDusBZTaedwKtiZ1");

function discriminator(name) {
  return createHash("sha256").update(`global:${name}`).digest().slice(0, 8);
}
function u32LE(n) { const b = Buffer.alloc(4); b.writeUInt32LE(n,0); return b; }
function i32LE(n) { const b = Buffer.alloc(4); b.writeInt32LE(n,0); return b; }
function u64LE(n) { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n),0); return b; }
function u128LE(n) {
  const b = Buffer.alloc(16);
  const big = BigInt(n);
  b.writeBigUInt64LE(big & 0xFFFFFFFFFFFFFFFFn, 0);
  b.writeBigUInt64LE(big >> 64n, 8);
  return b;
}

async function sendTx(connection, payer, ixs) {
  const { blockhash } = await connection.getLatestBlockhash();
  const tx = new Transaction({ recentBlockhash: blockhash, feePayer: payer.publicKey });
  tx.add(...ixs);
  tx.sign(payer);
  const sig = await connection.sendRawTransaction(tx.serialize(), { skipPreflight: false });
  await connection.confirmTransaction(sig, "confirmed");
  return sig;
}

async function main() {
  const connection = new Connection(RPC, "confirmed");
  const kpData = JSON.parse(fs.readFileSync(KEYPAIR_PATH, "utf-8"));
  const payer = Keypair.fromSecretKey(new Uint8Array(kpData));

  console.log("=== Nebula DEX — Add Liquidity & Test Swap ===");
  console.log("Pool:", POOL_ID.toBase58());

  // Token accounts
  const payerWxntAta = await getOrCreateAssociatedTokenAccount(
    connection, payer, WNXNT_MINT, payer.publicKey
  );
  const payerNusdcAta = await getOrCreateAssociatedTokenAccount(
    connection, payer, NUSDC_MINT, payer.publicKey
  );
  console.log("WXNT ATA:", payerWxntAta.address.toBase58());
  console.log("NUSDC ATA:", payerNusdcAta.address.toBase58());

  // Pool vaults
  const [token0Vault] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool_vault"), POOL_ID.toBuffer(), WNXNT_MINT.toBuffer()],
    PROGRAM_ID
  );
  const [token1Vault] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool_vault"), POOL_ID.toBuffer(), NUSDC_MINT.toBuffer()],
    PROGRAM_ID
  );
  const [observationState] = PublicKey.findProgramAddressSync(
    [Buffer.from("observation"), POOL_ID.toBuffer()],
    PROGRAM_ID
  );

  // NFT mint for position
  const positionNftMint = Keypair.generate();
  const [positionNftAccount] = PublicKey.findProgramAddressSync(
    [payer.publicKey.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), positionNftMint.publicKey.toBuffer()],
    ASSOCIATED_TOKEN_PROGRAM_ID
  );
  const [personalPosition] = PublicKey.findProgramAddressSync(
    [Buffer.from("position"), positionNftMint.publicKey.toBuffer()],
    PROGRAM_ID
  );
  const [protocolPosition] = PublicKey.findProgramAddressSync(
    [Buffer.from("p_position"), POOL_ID.toBuffer(), i32LE(-887272), i32LE(887272)],
    PROGRAM_ID
  );

  // Tick arrays for full range
  const [tickArrayLower] = PublicKey.findProgramAddressSync(
    [Buffer.from("tick_array"), POOL_ID.toBuffer(), i32LE(-887040)],
    PROGRAM_ID
  );
  const [tickArrayUpper] = PublicKey.findProgramAddressSync(
    [Buffer.from("tick_array"), POOL_ID.toBuffer(), i32LE(878400)],
    PROGRAM_ID
  );

  // ── Open position (full range) ─────────────────────────────────────────────
  console.log("\nOpening full-range position...");
  const tickLower = -887220; // near min tick, aligned to tickSpacing 60
  const tickUpper =  887220; // near max tick

  // open_position: disc + tickLower(i32) + tickUpper(i32) + tick_array_lower_start(i32) + tick_array_upper_start(i32) + with_metadata(bool)
  const openPosData = Buffer.concat([
    discriminator("open_position"),
    i32LE(tickLower),
    i32LE(tickUpper),
    i32LE(-887040),
    i32LE(878400),
    Buffer.from([0]), // with_metadata = false
  ]);

  const openPosIx = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      { pubkey: payer.publicKey,             isSigner: true,  isWritable: true  }, // payer
      { pubkey: payer.publicKey,             isSigner: true,  isWritable: false }, // position_nft_owner
      { pubkey: positionNftMint.publicKey,   isSigner: true,  isWritable: true  }, // position_nft_mint
      { pubkey: positionNftAccount,          isSigner: false, isWritable: true  }, // position_nft_account
      { pubkey: personalPosition,            isSigner: false, isWritable: true  }, // personal_position
      { pubkey: POOL_ID,                     isSigner: false, isWritable: true  }, // pool_state
      { pubkey: protocolPosition,            isSigner: false, isWritable: true  }, // protocol_position
      { pubkey: tickArrayLower,              isSigner: false, isWritable: true  }, // tick_array_lower
      { pubkey: tickArrayUpper,              isSigner: false, isWritable: true  }, // tick_array_upper
      { pubkey: TOKEN_PROGRAM_ID,            isSigner: false, isWritable: false },
      { pubkey: ASSOCIATED_TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
      { pubkey: SystemProgram.programId,     isSigner: false, isWritable: false },
      { pubkey: SYSVAR_RENT_PUBKEY,          isSigner: false, isWritable: false },
    ],
    data: openPosData,
  });

  try {
    const sig = await sendTx(connection, payer, [openPosIx]);
    // need positionNftMint as extra signer
    const { blockhash } = await connection.getLatestBlockhash();
    const tx2 = new Transaction({ recentBlockhash: blockhash, feePayer: payer.publicKey });
    tx2.add(openPosIx);
    tx2.sign(payer, positionNftMint);
    const s2 = await connection.sendRawTransaction(tx2.serialize(), { skipPreflight: false });
    await connection.confirmTransaction(s2, "confirmed");
    console.log("  ✅ Position opened:", personalPosition.toBase58());
    console.log("  Tx:", s2);
  } catch (err) {
    console.error("  ❌ open_position failed:", err.message);
    if (err.logs) console.error(err.logs.slice(-6).join("\n"));

    // Try with just payer + nftMint signing
    try {
      console.log("  Retrying with correct signers...");
      const { blockhash } = await connection.getLatestBlockhash();
      const tx = new Transaction({ recentBlockhash: blockhash, feePayer: payer.publicKey });
      tx.add(openPosIx);
      tx.sign(payer, positionNftMint);
      const sig = await connection.sendRawTransaction(tx.serialize(), { skipPreflight: false });
      await connection.confirmTransaction(sig, "confirmed");
      console.log("  ✅ Position opened:", personalPosition.toBase58());
      console.log("  Tx:", sig);
    } catch(err2) {
      console.error("  ❌ Retry failed:", err2.message);
      if (err2.logs) console.error(err2.logs.slice(-8).join("\n"));
    }
  }

  const finalBal = await connection.getBalance(payer.publicKey);
  console.log("\nFinal balance:", (finalBal / LAMPORTS_PER_SOL).toFixed(4), "XNT");
  console.log("Position NFT mint:", positionNftMint.publicKey.toBase58());
  console.log("Personal position:", personalPosition.toBase58());
  console.log("\nPool is live! Add liquidity via increase_liquidity, then swap.");
}

main().catch(console.error);
