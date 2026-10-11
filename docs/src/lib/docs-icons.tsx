import {
  BracketsCurlyIcon,
  CodeIcon,
  CubeIcon,
  DownloadSimpleIcon,
  EyeIcon,
  FileCodeIcon,
  FileTextIcon,
  GearIcon,
  TerminalWindowIcon,
  TestTubeIcon,
  TreeStructureIcon,
  WrenchIcon,
} from "@phosphor-icons/react/ssr";
import { McpIcon } from "@/components/mcp-icon";

const icons = {
  BracketsCurly: BracketsCurlyIcon,
  Code: CodeIcon,
  Cube: CubeIcon,
  DownloadSimple: DownloadSimpleIcon,
  Eye: EyeIcon,
  FileCode: FileCodeIcon,
  Gear: GearIcon,
  MCP: McpIcon,
  TerminalWindow: TerminalWindowIcon,
  TestTube: TestTubeIcon,
  TreeStructure: TreeStructureIcon,
  Wrench: WrenchIcon,
};

export function getDocsIcon(name?: string, size = 16) {
  const Icon =
    name && Object.hasOwn(icons, name) ? icons[name as keyof typeof icons] : FileTextIcon;
  return <Icon size={size} weight="light" aria-hidden="true" className="shrink-0" />;
}
