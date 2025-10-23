
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { LumenLaunch } from "../target/types/lumen_launch";

describe("lumen-launch", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.LumenLaunch as Program<LumenLaunch>;

  it("Can initialize a new token", async () => {
    const mintKeypair = anchor.web3.Keypair.generate();
    await program.rpc.initialize({
      accounts: {
        mint: mintKeypair.publicKey,
        authority: provider.wallet.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
      },
      signers: [mintKeypair],
    });
    const mintAccount = await program.account.mint.fetch(mintKeypair.publicKey);
    console.log("Mint created:", mintAccount);
  });
});
