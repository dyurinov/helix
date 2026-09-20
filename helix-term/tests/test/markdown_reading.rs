use super::*;
use std::io::Write;

#[tokio::test(flavor = "multi_thread")]
async fn reading_blocks_edits_and_allows_explicit_exit() -> anyhow::Result<()> {
    let mut file = tempfile::Builder::new().suffix(".md").tempfile()?;
    writeln!(file, "# Heading\n\nSome words to read.")?;
    let original = std::fs::read_to_string(file.path())?;
    let mut config = test_config();
    config.editor.markdown_reading_mode = true;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("ixdpcjkkggG"),
                Some(&|app| {
                    assert_eq!(helix_view::doc!(app.editor).text().to_string(), original);
                    assert_eq!(app.editor.mode(), helix_view::document::Mode::Normal);
                }),
            ),
            (
                Some("qiEDIT<esc>"),
                Some(&|app| {
                    assert!(helix_view::doc!(app.editor)
                        .text()
                        .to_string()
                        .starts_with("EDIT"));
                }),
            ),
        ],
        false,
    )
    .await?;
    assert_eq!(std::fs::read_to_string(file.path())?, original);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gw_leaves_reading_and_starts_word_jump() -> anyhow::Result<()> {
    let mut file = tempfile::Builder::new().suffix(".md").tempfile()?;
    writeln!(file, "# Heading\n\nSome words to read.")?;
    let mut config = test_config();
    config.editor.markdown_reading_mode = true;
    config.editor.jump_label_alphabet = vec!['a', 'b'];
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("gwaa"),
                Some(&|app| {
                    let (view, doc) = helix_view::current_ref!(app.editor);
                    assert_eq!(doc.selection(view.id).primary().from(), 2);
                    assert!(doc.text().to_string().starts_with("# Heading"));
                }),
            ),
            (
                Some("iEDIT<esc>"),
                Some(&|app| {
                    assert!(helix_view::doc!(app.editor)
                        .text()
                        .to_string()
                        .starts_with("# EDITHeading"));
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_can_be_reentered_after_unsaved_edit() -> anyhow::Result<()> {
    let mut file = tempfile::Builder::new().suffix(".md").tempfile()?;
    writeln!(file, "# Heading")?;
    let mut config = test_config();
    config.keys = toml::from_str::<helix_term::config::ConfigRaw>(
        "[keys.normal.space]\nm = 'markdown_reading_mode'",
    )?
    .keys
    .unwrap();
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("iEDIT<esc><space>mixd"),
                Some(&|app| {
                    assert_eq!(
                        helix_view::doc!(app.editor).text().to_string(),
                        "EDIT# Heading\n"
                    );
                }),
            ),
            (
                Some("qiMORE<esc>"),
                Some(&|app| {
                    assert!(helix_view::doc!(app.editor)
                        .text()
                        .to_string()
                        .contains("MORE"));
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn non_markdown_remains_editable() -> anyhow::Result<()> {
    let mut config = test_config();
    config.editor.markdown_reading_mode = true;
    test_with_config(
        AppBuilder::new().with_config(config),
        ("#[|text]#", "cchanged<esc>", "changed#[\n|]#"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn search_returns_to_the_matching_source_section() -> anyhow::Result<()> {
    let mut file = tempfile::Builder::new().suffix(".md").tempfile()?;
    writeln!(file, "<!-- Hidden comment -->\n# Beginning\n")?;
    for index in 0..50 {
        writeln!(file, "Paragraph {index}.\n")?;
    }
    writeln!(file, "Needle destination.\n")?;
    let original = std::fs::read_to_string(file.path())?;
    let mut config = test_config();
    config.editor.markdown_reading_mode = true;
    config.editor.jump_label_alphabet = vec!['a', 'b'];
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("/Needle<ret>gwaa"),
        Some(&|app| {
            let (view, doc) = helix_view::current_ref!(app.editor);
            assert_eq!(
                doc.selection(view.id).primary().from(),
                original.find("destination").unwrap()
            );
            assert_eq!(doc.text().to_string(), original);
        }),
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn heading_navigation_and_space_m_toggle_keep_position() -> anyhow::Result<()> {
    let mut file = tempfile::Builder::new().suffix(".md").tempfile()?;
    writeln!(
        file,
        "# First\n\nText.\n\n## Second\n\nMore.\n\n## Third\n\nEnd."
    )?;
    let original = std::fs::read_to_string(file.path())?;
    let mut config = test_config();
    config.editor.markdown_reading_mode = true;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("]]<space>m"),
        Some(&|app| {
            let (view, doc) = helix_view::current_ref!(app.editor);
            assert_eq!(
                doc.selection(view.id).primary().from(),
                original.find("## Third").unwrap()
            );
            assert_eq!(doc.text().to_string(), original);
        }),
        false,
    )
    .await
}
