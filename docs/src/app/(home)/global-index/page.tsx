import { GlobeHemisphereWestIcon } from "@phosphor-icons/react/ssr";
import type { Metadata } from "next";
import { GlobalIndex } from "@/components/global-index";

export const metadata: Metadata = {
  title: "Global Index",
  description: "Search and read public declarations and documentation for OSS packages.",
};

export default function Page() {
  return (
    <main className="mx-auto w-full max-w-360 px-6 py-10 sm:px-7">
      <div className="mb-8 max-w-190">
        <h1 className="flex items-center gap-3 font-mono text-3xl font-medium">
          <GlobeHemisphereWestIcon size={30} weight="light" aria-hidden="true" />
          Global Index
        </h1>
        <p className="mt-4 text-lg font-light leading-relaxed text-muted-foreground">
          The global index supplies public declarations and documentation for OSS packages. The API
          accepts package versions with each request.
        </p>
      </div>
      <GlobalIndex />
    </main>
  );
}
