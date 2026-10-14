import type { Metadata } from "next";

export const metadata: Metadata = { title: "Impressum" };

export default function ImpressumPage() {
  return (
    <main className="mx-auto w-full max-w-190 px-6 py-16 sm:px-7">
      <h1 className="mb-6 font-mono text-3xl">Impressum</h1>
      <div className="grid gap-4 text-muted-foreground">
        <p>
          Ivan Trusov operates these services and is responsible for their processing of personal
          data.
        </p>
        <p>Postal address: c/o GAM, Pappelallee 64, 10437 Berlin, Germany.</p>
        <p>
          Privacy contact:{" "}
          <a className="underline underline-offset-2" href="mailto:contact@volar.sh">
            contact@volar.sh
          </a>
          .
        </p>
      </div>
    </main>
  );
}
