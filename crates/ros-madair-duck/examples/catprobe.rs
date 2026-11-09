// throwaway probe: compare catalog concept_ids vs the melted concept store.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let head = &args[1];
    let gid = "0f366bdd-8e7f-519e-8ebc-ebff8a3e99a9"; // building
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let cat = format!("{}/concept_catalog.parquet", head);
    let mel = format!("{}/concepts_{}.parquet", head, gid);

    println!("== catalog rows for service roles ==");
    let mut s = conn.prepare(&format!("SELECT label, concept_id FROM read_parquet('{}') WHERE label IN ('Shelter','Hospital','Fire station')", cat)).unwrap();
    for r in s
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
    {
        let (l, i) = r.unwrap();
        println!("   {l}  ->  {i}");
    }

    println!("== melted store columns ==");
    let mut sc = conn
        .prepare(&format!(
            "SELECT column_name FROM (DESCRIBE SELECT * FROM read_parquet('{}'))",
            mel
        ))
        .unwrap();
    let cols: Vec<String> = sc
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|x| x.ok())
        .collect();
    println!("   {cols:?}");

    println!("== melted store: top concept_ids + counts ==");
    let mut s2 = conn.prepare(&format!("SELECT concept_id, count(*) c FROM read_parquet('{}') GROUP BY 1 ORDER BY c DESC LIMIT 8", mel)).unwrap();
    for r in s2
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
    {
        let (c, n) = r.unwrap();
        println!("   {c}  x{n}");
    }

    println!("== distinct node_ids in melted store ==");
    let mut s3 = conn
        .prepare(&format!(
            "SELECT DISTINCT node_id FROM read_parquet('{}') LIMIT 10",
            mel
        ))
        .unwrap();
    for r in s3.query_map([], |r| r.get::<_, String>(0)).unwrap() {
        println!("   {}", r.unwrap());
    }
}
