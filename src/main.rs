use cmark2tex::markdown_to_tex;
use fs::OpenOptions;
use fs_err as fs;
use mdbook::book::BookItem;
use mdbook::renderer::RenderContext;
use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag};
use pulldown_cmark_to_cmark::cmark;
use std::io::{self, BufReader, Write};
use std::path::Path;
use std::path::PathBuf;

#[cfg(test)]
mod tests;

// config definition.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LatexConfig {
    // Chapters that will not be exported.
    pub ignores: Vec<String>,

    // Output latex file.
    pub latex: bool,

    // Output PDF.
    pub pdf: bool,

    // Output markdown file.
    pub markdown: bool,

    // Use user's LaTeX template file instead of default (template.tex).
    pub custom_template: Option<String>,

    // Date to be used in the LaTeX \date{} macro
    #[serde(default = "today")]
    pub date: String,
}

fn today() -> String {
    r#"\today"#.to_owned()
}

impl Default for LatexConfig {
    fn default() -> Self {
        Self {
            ignores: Default::default(),
            latex: true,
            pdf: true,
            markdown: true,
            custom_template: None,
            date: today(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[error("Failed to parse STDIN as `RenderContext` JSON: {0:?}")]
struct Error(#[from] mdbook::errors::Error);

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    // eprintln!("MDBOOK TECTONIC IS INVOKED");
    let stdin = BufReader::new(io::stdin());

    // Get markdown source from the mdbook command via stdin
    let ctx = RenderContext::from_json(stdin).map_err(Error)?;

    let compiled_against = semver::VersionReq::parse(mdbook::MDBOOK_VERSION)?;
    let running_against = semver::Version::parse(ctx.version.as_str())?;
    if !compiled_against.matches(&running_against) {
        // We should probably use the `semver` crate to check compatibility
        // here...
        eprintln!(
            "Warning: The {} output was built against version {} of mdbook, \
             but we're being called from version {}",
            "tectonic",
            mdbook::MDBOOK_VERSION,
            ctx.version
        );
    }

    // Get configuration options from book.toml.
    let cfg: LatexConfig = ctx
        .config
        .get_deserialized_opt("output.latex")
        .expect("Error reading \"output.latex\" configuration")
        .unwrap_or_default();

    // Read book's config values (title, authors).
    let title = ctx
        .config
        .book
        .title
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or("<Unknown Title>");
    let authors = ctx.config.book.authors.join(" \\and ");
    let date = cfg.date.clone();

    // Copy template data into memory.
    let mut template = if let Some(custom_template) = cfg.custom_template {
        let mut custom_template_path = ctx.root.clone();
        custom_template_path.push(custom_template);
        fs::read_to_string(custom_template_path)?
    } else {
        include_str!("template.tex").to_string()
    };

    // Add title and author information.
    template = template.replace(r"\title{}", &format!("\\title{{{}}}", title));
    template = template.replace(r"\author{}", &format!("\\author{{{}}}", authors));
    template = template.replace(r"\date{}", &format!("\\date{{{}}}", date));

    let mut latex = String::new();

    // Iterate through markdown source and push the chapters onto one single string.
    let mut content = String::new();
    for item in ctx.book.iter() {
        // Iterate through each chapter.
        if let BookItem::Chapter(ref ch) = *item {
            if cfg.ignores.contains(&ch.name) {
                continue;
            }

            // Add chapter path to relative links.
            content.push_str(&traverse_markdown(
                &ch.content,
                ch.path.as_ref().unwrap().parent().unwrap(),
                &ctx,
            ));
        }
    }

    // println!("{}", content);
    if cfg.markdown {
        // Output markdown file.
        output_markdown(".md", title, &content, &ctx.destination)?;
    }

    if cfg.latex || cfg.pdf {
        // convert markdown data to LaTeX
        latex.push_str(&markdown_to_tex(content)?);

        // Insert new LaTeX data into template after "%% mdbook-tectonic begin".
        let begin = "mdbook-tectonic begin";
        let pos = template.find(&begin).unwrap() + begin.len();
        template.insert_str(pos, &latex);

        if cfg.latex {
            // Output latex file.
            output_markdown(".tex", title, &template, &ctx.destination)?;
        }

        // Output PDF file.
        if cfg.pdf {
            // let mut input = tempfile::NamedTempFile::new()?;
            // input.write(template.as_bytes())?;

            // Write PDF with tectonic.
            println!("Writing PDF with Tectonic...");
            // FIXME launch tectonic process
            let tectonic = which::which("tectonic")?;
            let mut child = std::process::Command::new(tectonic)
                .arg("--outfmt=pdf")
                .arg(format!("-o={}", std::env::current_dir()?.display()))
                .arg("-")
                .stdin(std::process::Stdio::piped())
                .spawn()?;
            {
                let mut tectonic_stdin = child.stdin.as_mut().unwrap();
                let mut tectonic_writer = std::io::BufWriter::new(&mut tectonic_stdin);
                tectonic_writer.write(template.as_bytes())?;
            }
            if child.wait()?.code().unwrap() != 0 {
                panic!("BAAAAAAAAD");
            }
            // let pdf_data: Vec<u8> = tectonic::latex_to_pdf(&template).expect("processing failed");
            // println!("Output PDF size is {} bytes", pdf_data.len());
        }
    }

    Ok(())
}

/// Output plain text file.
///
/// Used for writing markdown and latex data to files.
fn output_markdown<P: AsRef<Path>>(
    extension: &str,
    filename: &str,
    data: &str,
    destination: P,
) -> Result<(), io::Error> {
    let mut path = PathBuf::from(filename);
    path.set_extension(extension);

    // Create output directory/file.
    fs::create_dir_all(destination)?;

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)?;
    file.write_all(data.as_bytes())?;
    Ok(())
}

/// This Function parses the markdown file, alters some elements and writes it back to markdown.
///
/// Changes done:
///   * change image paths to be relative to images
///   * copy the image files into the images directory in the target directory
fn traverse_markdown(content: &str, chapter_path: &Path, context: &RenderContext) -> String {
    let parser = Parser::new_ext(content, Options::all());
    let parser = parser.map(|event| match event {
        Event::Start(Tag::Image(link_type, path, title)) => {
            //Event::Start(Tag::Image(link_type, imagepathcowstr, title))
            Event::Start(parse_image_tag(
                link_type,
                path,
                title,
                chapter_path,
                context,
            ))
        }
        Event::End(Tag::Image(link_type, path, title)) => {
            //Event::Start(Tag::Image(link_type, imagepathcowstr, title))
            Event::End(parse_image_tag(
                link_type,
                path,
                title,
                chapter_path,
                context,
            ))
        }
        _ => event,
    });
    let mut new_content = String::new();

    cmark(parser, &mut new_content).expect("failed to convert back to markdown");
    return new_content;
}

fn parse_image_tag<'a>(
    link_type: LinkType,
    path: CowStr<'a>,
    title: CowStr<'a>,
    chapter_path: &'a Path,
    context: &'a RenderContext,
) -> Tag<'a> {
    //! Take the values of a Tag::Image and create a new Tag::Image
    //! while simplyfying the path and also copying the image file to the target directory

    // cleaning and converting the path found.
    let pathstr: String = path.replace("./", "");
    let imagefn = Path::new(&pathstr);
    // creating the source path of the mdbook
    let source = context.root.join(context.config.book.src.clone());
    // creating the relative path of the image by prepending the chapterpath

    let relpath = chapter_path.join(imagefn);
    // creating the path of the imagesource
    let sourceimage = source.join(&relpath);
    // creating the relative path for the image tag in markdown
    let imagepath = Path::new("images").join(&relpath);
    // creating the path where the image will be copied to
    let targetimage = context.destination.join(&imagepath);

    // creating the directory if neccessary
    fs::create_dir_all(targetimage.parent().unwrap()).expect("Failed to create the directories");
    // copy the image
    fs::copy(&sourceimage, &targetimage).expect("Failed to copy the image");
    // create the new image
    let imagepathc: String = imagepath.to_str().unwrap().into();
    Tag::Image(link_type, imagepathc.into(), title)
}
