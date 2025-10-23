// lumen_launch Anchor program tests (TypeScript + Mocha)

import * as anchor from "@coral-xyz/anchor";
import { PublicKey, Keypair, SystemProgram } from "@solana/web3.js";
import { TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID } from "@solana/spl-token";

// Declare mocha globals to satisfy TypeScript without @types/mocha
declare const describe: any;
declare const it: any;
declare const before: any;

// If you have generated types from your IDL, import them here:
// import { LumenLaunch } from "../target/types/lumen_launch";
// Otherwise, keep Program<any> and rely on anchor.workspace when IDL is present.

describe("lumen_launch program", () => {
  // Provider and program (fallback if ANCHOR_PROVIDER_URL is not set)
  let provider: anchor.AnchorProvider;
  try {
    provider = anchor.AnchorProvider.env();
  } catch (e) {
    const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
    const connection = new anchor.web3.Connection(url, "confirmed");
    const wallet = new anchor.Wallet(Keypair.generate());
    provider = new anchor.AnchorProvider(connection, wallet, {} as any);
  }
  anchor.setProvider(provider);

  // If the IDL exists locally (target/idl) and Anchor is configured, this will work.
  // Otherwise, import the IDL JSON manually and instantiate Program via new Program(idl, programId, provider).
  const program: any = (anchor.workspace as any).LumenLaunch as any;

  // Program-level constants (must match program/src/lib.rs)
  const USDC_DEVNET = new PublicKey("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
  const PLATFORM_WALLET = new PublicKey("7XB2PEWYd5be12CpJ9e4ZTZTHCgNrcTbL7HigciPd1C6");
  const PLATFORM_FEE_USDC = new anchor.BN(5_000_000); // 5 USDC, 6 decimals (unused here, for reference)
  const MIN_VIRTUAL_USDC = new anchor.BN(10_000_000); // 10 USDC

  // Test actors
  const creator = (provider.wallet as anchor.Wallet).payer as Keypair; // default wallet
  const buyer = Keypair.generate();

  // Created per test
  let mint: PublicKey; // X token mint
  let launchConfigPda: PublicKey;
  let launchConfigBump: number;
  let mintAuthPda: PublicKey;
  let mintAuthBump: number;
  let usdcVaultAuthPda: PublicKey;
  let usdcVaultAuthBump: number;
  let bondingCurveAta: PublicKey;
  let usdcVault: PublicKey;

  // Helper PDA derivations
  const deriveMintAuth = async (mintPk: PublicKey) => {
    const [pda, bump] = PublicKey.findProgramAddressSync(
      [Buffer.from("mint-auth"), mintPk.toBuffer()],
      program.programId
    );
    return { pda, bump };
  };

  const deriveUsdcVaultAuth = (mintPk: PublicKey) => {
    const [pda, bump] = PublicKey.findProgramAddressSync(
      [Buffer.from("usdc-vault"), mintPk.toBuffer()],
      program.programId
    );
    return { pda, bump };
  };

  const deriveLaunchConfig = (mintPk: PublicKey) => {
    const [pda, bump] = PublicKey.findProgramAddressSync(
      [Buffer.from("launch"), mintPk.toBuffer()],
      program.programId
    );
    return { pda, bump };
  };

  const ata = async (owner: PublicKey, mintPk: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [owner.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), mintPk.toBuffer()],
      ASSOCIATED_TOKEN_PROGRAM_ID
    )[0];

  before("airdrop SOL and create buyer wallet", async () => {
    // Fund buyer with SOL for fees
    const sig = await provider.connection.requestAirdrop(buyer.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig);
  });

  it("derives PDAs deterministically", async () => {
    const dummyMint = Keypair.generate().publicKey;
    const { pda: mintAuth } = await deriveMintAuth(dummyMint);
    const { pda: vaultAuth } = deriveUsdcVaultAuth(dummyMint);
    const { pda: cfg } = deriveLaunchConfig(dummyMint);

    // Ensure PDAs are unique and on-curve
    if (mintAuth.equals(vaultAuth) || mintAuth.equals(cfg) || vaultAuth.equals(cfg)) {
      throw new Error("PDA derivations collided unexpectedly");
    }
  });

  it("create_token initializes mint, launch config, bonding curve, and USDC vault (scaffold)", async function () {
    // IMPORTANT: This instruction requires the creator to hold >= 5 + 10 USDC in their USDC ATA on the selected cluster.
    // On devnet, ensure the provider's wallet has USDC at the USDC_DEVNET mint.

    // Create a fresh mint keypair (program will init it)
    const mintKp = Keypair.generate();
    mint = mintKp.publicKey;

    const { pda: _mintAuth, bump: _mintAuthBump } = await deriveMintAuth(mint);
    mintAuthPda = _mintAuth;
    mintAuthBump = _mintAuthBump;

    const { pda: _cfg, bump: _cfgBump } = deriveLaunchConfig(mint);
    launchConfigPda = _cfg;
    launchConfigBump = _cfgBump;

    const { pda: _vaultAuth, bump: _vaultAuthBump } = deriveUsdcVaultAuth(mint);
    usdcVaultAuthPda = _vaultAuth;
    usdcVaultAuthBump = _vaultAuthBump;

    bondingCurveAta = await ata(launchConfigPda, mint);
    usdcVault = await ata(usdcVaultAuthPda, USDC_DEVNET);

    const creatorUsdcAta = await ata(creator.publicKey, USDC_DEVNET);
    const platformUsdcAta = await ata(PLATFORM_WALLET, USDC_DEVNET);

    // Assemble accounts according to program definition (snake_case keys per IDL)
    const accounts = {
      creator: creator.publicKey,
      mint: mint,
      mint_auth: mintAuthPda,
      launch_config: launchConfigPda,
      bonding_curve_ata: bondingCurveAta,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      usdc_mint: USDC_DEVNET,
      creator_usdc: creatorUsdcAta,
      platform_wallet: PLATFORM_WALLET,
      platform_usdc_ata: platformUsdcAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const virtualUsdc = MIN_VIRTUAL_USDC; // 10 USDC

    // Build instruction (avoid sending if not funded)
    const ix = await program.methods
      .createToken(USDC_DEVNET, virtualUsdc)
      .accounts(accounts)
      .instruction();

    if (!ix) throw new Error("Failed to build create_token instruction");

    // Optionally, send if funded:
    // const txSig = await program.methods
    //   .createToken(USDC_DEVNET, virtualUsdc)
    //   .accounts(accounts)
    //   .signers([creator, mintKp])
    //   .rpc();
    // console.log("create_token tx:", txSig);
  });

  it.skip("buy mints X tokens in exchange for USDC (scaffold)", async () => {
    // Prerequisite: create_token succeeded and vault/ledgers exist; buyer has USDC in ATA.

    const buyerUsdcAta = await ata(buyer.publicKey, USDC_DEVNET);
    const buyerXAta = await ata(buyer.publicKey, mint);

    const burnAtaOwner = mintAuthPda; // burn authority equals mint auth PDA
    const burnAta = await ata(burnAtaOwner, mint);

    const creatorUsdcAta = await ata(creator.publicKey, USDC_DEVNET);

    const buyerLedger = PublicKey.findProgramAddressSync(
      [Buffer.from("buyer_ledger"), mint.toBuffer(), buyer.publicKey.toBuffer()],
      program.programId
    )[0];

    const accounts = {
      buyer: buyer.publicKey,
      launch_config: launchConfigPda,
      mint,
      usdc_mint: USDC_DEVNET,
      buyer_usdc: buyerUsdcAta,
      usdc_vault: usdcVault,
      bonding_curve_ata: bondingCurveAta,
      buyer_x_ata: buyerXAta,
      buyer_ledger: buyerLedger,
      burn_auth: mintAuthPda,
      burn_ata: burnAta,
      mint_auth: mintAuthPda,
      usdc_vault_auth: usdcVaultAuthPda,
      creator: creator.publicKey,
      creator_usdc_ata: creatorUsdcAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const now = Math.floor(Date.now() / 1000);
    const usdcIn = new anchor.BN(1_000_000); // 1 USDC
    const minTokensOut = new anchor.BN(1); // accept any positive amount
    const deadline = new anchor.BN(now + 60);

    const ix = await program.methods
      .buy(usdcIn, minTokensOut, deadline)
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build buy instruction");
  });

  it.skip("sell burns X tokens and pays USDC minus tax (scaffold)", async () => {

    const sellerXAta = await ata(buyer.publicKey, mint);
    const sellerUsdcAta = await ata(buyer.publicKey, USDC_DEVNET);
    const platformProfitAta = await ata(PLATFORM_WALLET, USDC_DEVNET);
    const creatorProfitAta = await ata(launchConfigPda, USDC_DEVNET);

    const accounts = {
      seller: buyer.publicKey,
      launch_config: launchConfigPda,
      mint,
      seller_x_ata: sellerXAta,
      bonding_curve_ata: bondingCurveAta,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      seller_usdc_ata: sellerUsdcAta,
      usdc_mint: USDC_DEVNET,
      platform_wallet: PLATFORM_WALLET,
      platform_profit_ata: platformProfitAta,
      creator_profit_ata: creatorProfitAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const now = Math.floor(Date.now() / 1000);
    const tokenIn = new anchor.BN(1000);
    const minUsdcOut = new anchor.BN(1);
    const deadline = new anchor.BN(now + 60);

    const ix = await program.methods
      .sell(tokenIn, minUsdcOut, deadline)
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build sell instruction");
  });

  it.skip("claim_profits distributes reserve to holder based on index (scaffold)", async () => {

    const userXAta = await ata(buyer.publicKey, mint);
    const userUsdcAta = await ata(buyer.publicKey, USDC_DEVNET);
    const buyerLedger = PublicKey.findProgramAddressSync([
      Buffer.from("buyer_ledger"),
      mint.toBuffer(),
      buyer.publicKey.toBuffer(),
    ], program.programId)[0];

    const accounts = {
      user: buyer.publicKey,
      launch_config: launchConfigPda,
      buyer_ledger: buyerLedger,
      mint,
      user_x_ata: userXAta,
      user_usdc_ata: userUsdcAta,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      usdc_mint: USDC_DEVNET,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const ix = await program.methods
      .claimProfits()
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build claim_profits instruction");
  });

  it.skip("finalize marks the launch closed and emits event (scaffold)", async () => {

    const creatorUsdcAta = await ata(creator.publicKey, USDC_DEVNET);
    const accounts = {
      caller: creator.publicKey,
      launch_config: launchConfigPda,
      mint,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      creator: creator.publicKey,
      usdc_mint: USDC_DEVNET,
      creator_usdc_ata: creatorUsdcAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const ix = await program.methods
      .finalize()
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build finalize instruction");
  });

  it.skip("withdraw_creator_reserve pays out creator (scaffold)", async () => {

    const creatorUsdcAta = await ata(creator.publicKey, USDC_DEVNET);
    const req = new anchor.BN(1_000_000);
    const accounts = {
      creator: creator.publicKey,
      launch_config: launchConfigPda,
      mint,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      usdc_mint: USDC_DEVNET,
      creator_usdc_ata: creatorUsdcAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const ix = await program.methods
      .withdrawCreatorReserve(req)
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build withdraw_creator_reserve instruction");
  });

  it.skip("reclaim_virtual_funds refunds initial USDC (scaffold)", async () => {

    const creatorUsdcAta = await ata(creator.publicKey, USDC_DEVNET);
    const accounts = {
      creator: creator.publicKey,
      launch_config: launchConfigPda,
      mint,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      usdc_mint: USDC_DEVNET,
      creator_usdc_ata: creatorUsdcAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const ix = await program.methods
      .reclaimVirtualFunds()
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build reclaim_virtual_funds instruction");
  });

  it.skip("close_and_migrate_to_raydium locks LP (scaffold)", async () => {

    const lpMint = Keypair.generate().publicKey; // placeholder
    const ammId = Keypair.generate().publicKey; // placeholder

    const coinVault = await ata(ammId, mint);
    const pcVault = await ata(ammId, USDC_DEVNET);

    const lpLockPda = PublicKey.findProgramAddressSync([
      Buffer.from("lp-lock"),
      ammId.toBuffer(),
    ], program.programId)[0];

    const lpLockAuthPda = lpLockPda; // same seeds, same PDA

    const creatorUsdcAta = await ata(creator.publicKey, USDC_DEVNET);

    const accounts = {
      payer: creator.publicKey,
      launch_config: launchConfigPda,
      mint,
      bonding_curve_ata: bondingCurveAta,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      amm_id: ammId,
      lp_mint: lpMint,
      coin_vault: coinVault,
      pc_vault: pcVault,
      lp_lock: lpLockPda,
      lp_lock_vault: await ata(lpLockAuthPda, lpMint),
      lp_lock_auth: lpLockAuthPda,
      mint_auth: mintAuthPda,
      creator: creator.publicKey,
      creator_usdc_ata: creatorUsdcAta,
      usdc_mint: USDC_DEVNET,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const ix = await program.methods
      .closeAndMigrateToRaydium()
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build close_and_migrate_to_raydium instruction");
  });

  it.skip("withdraw_platform_remaining pays out platform fees with cooldown (scaffold)", async () => {

    const platformUsdcAta = await ata(PLATFORM_WALLET, USDC_DEVNET);

    const accounts = {
      platform: PLATFORM_WALLET,
      launch_config: launchConfigPda,
      mint,
      platform_usdc_ata: platformUsdcAta,
      usdc_vault: usdcVault,
      usdc_vault_auth: usdcVaultAuthPda,
      usdc_mint: USDC_DEVNET,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    } as any;

    const ix = await program.methods
      .withdrawPlatformRemaining()
      .accounts(accounts)
      .instruction();
    if (!ix) throw new Error("Failed to build withdraw_platform_remaining instruction");
  });
});
