import { RootToggle } from "fumadocs-ui/components/layout/root-toggle";
import { DOC_VERSIONS, DRAFT } from "@/lib/versions";

/**
 * The version flipper, in the sidebar footer rather than the banner slot the
 * derived tabs would occupy. Options mirror the root folders per version.
 */
export function VersionSwitch() {
  return (
    <RootToggle
      className="version-switch-trigger w-full"
      options={DOC_VERSIONS.map((version) => ({
        title: version === DRAFT ? "Draft" : version,
        url: `/docs/${version}`,
        props: { className: "version-switch-item" },
      }))}
    />
  );
}
