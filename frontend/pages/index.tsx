import { useEffect } from "react";

export default function Home() {
  useEffect(() => {
    console.log("Lumen Launch Frontend loaded");
  }, []);

  return (
    <div className="p-4">
      <h1 className="text-2xl font-bold">Lumen Launch Platform</h1>
      <p>Frontend placeholder for your Solana launchpad.</p>
    </div>
  );
}
