const render = <T,>(value: T) => <div>{value}</div>;

export function App(): unknown {
  return <main>{render("beacon")}</main>;
}
