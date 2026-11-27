import registry from "../../public/errors.json";

export function ErrorRegistry() {
  const namespaces = new Map<string, typeof registry.errors>();
  for (const error of registry.errors) {
    const [, namespace] = error.slug.split(".");
    const entries = namespaces.get(namespace);
    if (entries) entries.push(error);
    else namespaces.set(namespace, [error]);
  }

  return (
    <>
      <a href={`${process.env.NEXT_PUBLIC_BASE_PATH ?? ""}/errors.json`}>Error registry JSON</a>
      <nav aria-label="Error namespaces" className="not-prose my-4 flex flex-wrap gap-2">
        {Array.from(namespaces.keys(), (namespace) => (
          <a
            key={namespace}
            href={`#${namespace}`}
            className="border border-fd-border px-2 py-1 font-mono text-sm hover:bg-fd-accent"
          >
            {namespace}
          </a>
        ))}
      </nav>
      {Array.from(namespaces, ([namespace, errors]) => (
        <section key={namespace} aria-labelledby={namespace}>
          <h2 id={namespace} className="scroll-mt-20">
            {namespace}
          </h2>
          <ul className="not-prose divide-y divide-fd-border border-y border-fd-border">
            {errors.map(({ slug, message, action, fields }) => (
              <li key={slug} id={slug} className="scroll-mt-20 py-3">
                <a href={`#${slug}`} className="break-all font-mono text-sm hover:underline">
                  {slug}
                </a>
                <p className="mt-1">{message}</p>
                {action && <p className="mt-1 text-fd-muted-foreground">{action}</p>}
                {fields.length > 0 && (
                  <details className="mt-2 text-sm">
                    <summary className="cursor-pointer">Evidence fields</summary>
                    <ul className="mt-1 space-y-1 pl-4">
                      {fields.map(({ name, kind, optional, display, sensitive }) => (
                        <li key={name}>
                          <code>{name}</code>{" "}
                          <span className="ml-2 text-fd-muted-foreground">
                            {[
                              kind,
                              optional ? "optional" : "required",
                              !display && "hidden",
                              sensitive && "sensitive",
                            ]
                              .filter(Boolean)
                              .join(", ")}
                          </span>
                        </li>
                      ))}
                    </ul>
                  </details>
                )}
              </li>
            ))}
          </ul>
        </section>
      ))}
    </>
  );
}
