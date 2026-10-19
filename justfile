# render the github pages site into _site/ and open it
site:
    rm -rf _site && mkdir _site
    cp screenshot.png site.css flatpak/io.github.andunieee.twain.svg _site/
    printf '<link rel="icon" href="io.github.andunieee.twain.svg">\n' > _site/header.html
    pandoc README.md --standalone --css site.css --metadata pagetitle=twain \
        --include-in-header=_site/header.html -o _site/index.html
    rm _site/header.html
    xdg-open _site/index.html
