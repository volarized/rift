import { createRelativeLink } from "fumadocs-ui/mdx";
import { DocsBody, DocsDescription, DocsPage, DocsTitle } from "fumadocs-ui/page";
import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { BackToTop } from "@/components/back-to-top";
import { allPageData } from "@/lib/protocol-surface";
import { source } from "@/lib/source";
import { getMDXComponents } from "@/mdx-components";

export default async function Page(props: PageProps<"/docs/[[...slug]]">) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDXContent = page.data.body;

  // A protocol page is hand-written prose followed by a component that renders
  // the schemas. remark sees only the prose, so the component's headings are
  // appended from the same derivation that produced them — see `pageData`.
  //
  // A heading written in the MDX *and* claimed by the derivation would appear
  // twice and React would see two children with one key, so the MDX wins: it is
  // the half remark actually rendered an anchor for.
  const derived = allPageData[page.url];
  const seen = new Set(page.data.toc.map((item) => item.url));
  const toc = derived
    ? [...page.data.toc, ...derived.toc.filter((item) => !seen.has(item.url))]
    : page.data.toc;

  return (
    <DocsPage
      toc={toc}
      full={page.data.full}
      // `single: false` keeps the highlight tracking sections as they cross
      // the upper half of the viewport; with no headings, keep the column but
      // render nothing into it.
      tableOfContent={
        toc.length > 0
          ? { single: false, footer: <BackToTop /> }
          : { enabled: true, component: <></> }
      }
    >
      <DocsTitle>{page.data.title}</DocsTitle>
      <DocsDescription>{page.data.description}</DocsDescription>
      <DocsBody>
        <MDXContent
          components={getMDXComponents({
            // this allows you to link to other pages with relative file paths
            a: createRelativeLink(source, page),
          })}
        />
      </DocsBody>
    </DocsPage>
  );
}

export async function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(props: PageProps<"/docs/[[...slug]]">): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return {
    title: page.data.title,
    description: page.data.description,
  };
}
