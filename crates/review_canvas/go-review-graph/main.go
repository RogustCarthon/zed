// Command go-review-graph loads Go packages and writes their top-level declarations, with the
// declarations that each one uses, as JSON. The Zed review canvas uses the output to order and
// connect the changed declarations of a branch.
//
// Usage: go-review-graph <dir> <import path>...
// Output (stdout): {"packages": [...], "errors": [...]}
package main

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/token"
	"go/types"
	"os"
	"sort"
	"strings"

	"golang.org/x/tools/go/packages"
)

type response struct {
	Packages []packageOutput `json:"packages"`
	Errors   []string        `json:"errors"`
}

type packageOutput struct {
	// ID is the package path. The package path of an external test package ends with _test.
	ID             string        `json:"id"`
	IsExternalTest bool          `json:"is_external_test"`
	Declarations   []declaration `json:"declarations"`
}

type declaration struct {
	ID        string   `json:"id"`
	Name      string   `json:"name"`
	Kind      string   `json:"kind"`
	File      string   `json:"file"`
	StartLine int      `json:"start_line"`
	EndLine   int      `json:"end_line"`
	Uses      []string `json:"uses"`
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "go-review-graph:", err)
		os.Exit(1)
	}
}

func run() error {
	if len(os.Args) < 2 {
		return fmt.Errorf("usage: go-review-graph <dir> <import path>...")
	}
	dir, importPaths := os.Args[1], os.Args[2:]
	if len(importPaths) == 0 {
		return json.NewEncoder(os.Stdout).Encode(response{Packages: []packageOutput{}, Errors: []string{}})
	}

	fset := token.NewFileSet()
	config := &packages.Config{
		Mode: packages.NeedName | packages.NeedFiles | packages.NeedSyntax |
			packages.NeedTypes | packages.NeedTypesInfo | packages.NeedImports,
		Dir:   dir,
		Tests: true,
		Fset:  fset,
	}
	loaded, err := packages.Load(config, importPaths...)
	if err != nil {
		return fmt.Errorf("loading packages: %w", err)
	}

	chosen := choosePackages(loaded)
	resp := response{Packages: []packageOutput{}, Errors: []string{}}
	for _, pkg := range chosen {
		for _, packageError := range pkg.Errors {
			resp.Errors = append(resp.Errors, packageError.Error())
		}
	}

	// Objects can differ between package variants, so aliases are found by position.
	aliases := newAliases()
	for _, pkg := range chosen {
		aliases.collect(pkg)
	}
	for _, pkg := range chosen {
		resp.Packages = append(resp.Packages, describePackage(pkg, aliases))
	}
	keepKnownUses(resp.Packages)
	sort.Slice(resp.Packages, func(i, j int) bool { return resp.Packages[i].ID < resp.Packages[j].ID })
	return json.NewEncoder(os.Stdout).Encode(resp)
}

// keepKnownUses removes the uses of declarations outside the loaded packages, for example of the
// standard library. The review canvas only connects declarations that it knows.
func keepKnownUses(outputs []packageOutput) {
	known := map[string]bool{}
	for _, output := range outputs {
		for _, decl := range output.Declarations {
			known[decl.ID] = true
		}
	}
	for _, output := range outputs {
		for index := range output.Declarations {
			uses := output.Declarations[index].Uses[:0]
			for _, use := range output.Declarations[index].Uses {
				if known[use] {
					uses = append(uses, use)
				}
			}
			output.Declarations[index].Uses = uses
		}
	}
}

// choosePackages keeps one variant per package path. A test variant ("p [p.test]") has all files
// of the plain variant plus the internal test files, so it wins. Test main packages are dropped.
func choosePackages(loaded []*packages.Package) []*packages.Package {
	byPath := map[string]*packages.Package{}
	for _, pkg := range loaded {
		if strings.HasSuffix(pkg.PkgPath, ".test") || pkg.TypesInfo == nil {
			continue
		}
		current, ok := byPath[pkg.PkgPath]
		if !ok || len(pkg.Syntax) > len(current.Syntax) {
			byPath[pkg.PkgPath] = pkg
		}
	}
	chosen := make([]*packages.Package, 0, len(byPath))
	for _, pkg := range byPath {
		chosen = append(chosen, pkg)
	}
	sort.Slice(chosen, func(i, j int) bool { return chosen[i].PkgPath < chosen[j].PkgPath })
	return chosen
}

func positionKey(fset *token.FileSet, pos token.Pos) string {
	position := fset.Position(pos)
	return fmt.Sprintf("%s:%d:%d", position.Filename, position.Line, position.Column)
}

// aliases maps objects that are not declarations themselves to the declaration that holds them.
type aliases struct {
	// The position of a struct field -> the ID of the type that declares it.
	fieldOwners map[string]string
	// The position of a later name in a value spec -> the first name. All names of
	// `var a, b = 1, 2` use the declaration ID of `a`.
	valueSpecFirstNames map[string]string
}

func newAliases() *aliases {
	return &aliases{fieldOwners: map[string]string{}, valueSpecFirstNames: map[string]string{}}
}

func (a *aliases) collect(pkg *packages.Package) {
	for _, file := range pkg.Syntax {
		for _, decl := range file.Decls {
			genDecl, ok := decl.(*ast.GenDecl)
			if !ok {
				continue
			}
			if genDecl.Tok == token.VAR || genDecl.Tok == token.CONST {
				for _, spec := range genDecl.Specs {
					names := spec.(*ast.ValueSpec).Names
					for _, name := range names[1:] {
						a.valueSpecFirstNames[positionKey(pkg.Fset, name.Pos())] = names[0].Name
					}
				}
				continue
			}
			if genDecl.Tok != token.TYPE {
				continue
			}
			for _, spec := range genDecl.Specs {
				typeSpec := spec.(*ast.TypeSpec)
				owner := pkg.PkgPath + "." + typeSpec.Name.Name
				ast.Inspect(typeSpec.Type, func(node ast.Node) bool {
					field, ok := node.(*ast.Field)
					if !ok {
						return true
					}
					for _, name := range field.Names {
						a.fieldOwners[positionKey(pkg.Fset, name.Pos())] = owner
					}
					if len(field.Names) == 0 {
						// An embedded field: its position is the position of its type expression.
						a.fieldOwners[positionKey(pkg.Fset, embeddedFieldPos(field.Type))] = owner
					}
					return true
				})
			}
		}
	}
}

func embeddedFieldPos(expr ast.Expr) token.Pos {
	switch expr := expr.(type) {
	case *ast.StarExpr:
		return embeddedFieldPos(expr.X)
	case *ast.SelectorExpr:
		return expr.Sel.Pos()
	case *ast.IndexExpr:
		return embeddedFieldPos(expr.X)
	case *ast.IndexListExpr:
		return embeddedFieldPos(expr.X)
	default:
		return expr.Pos()
	}
}

func describePackage(pkg *packages.Package, aliases *aliases) packageOutput {
	output := packageOutput{
		ID:             pkg.PkgPath,
		IsExternalTest: strings.HasSuffix(pkg.Name, "_test") && strings.HasSuffix(pkg.PkgPath, "_test"),
		Declarations:   []declaration{},
	}
	seen := map[string]bool{}
	add := func(decl declaration, node ast.Node) {
		if seen[decl.ID] {
			decl.ID = fmt.Sprintf("%s#%s:%d", decl.ID, decl.File, decl.StartLine)
		}
		seen[decl.ID] = true
		decl.Uses = collectUses(pkg, node, decl.ID, aliases)
		output.Declarations = append(output.Declarations, decl)
	}

	for _, file := range pkg.Syntax {
		filename := pkg.Fset.Position(file.Pos()).Filename
		lines := func(start, end token.Pos) (int, int) {
			return pkg.Fset.Position(start).Line, pkg.Fset.Position(end).Line
		}
		for _, decl := range file.Decls {
			switch decl := decl.(type) {
			case *ast.FuncDecl:
				start := decl.Pos()
				if decl.Doc != nil {
					start = decl.Doc.Pos()
				}
				startLine, endLine := lines(start, decl.End())
				id, kind := funcID(pkg.PkgPath, decl)
				if decl.Name.Name == "init" && decl.Recv == nil {
					id = fmt.Sprintf("%s#%s:%d", id, filename, startLine)
				}
				add(declaration{
					ID: id, Name: funcName(decl), Kind: kind, File: filename,
					StartLine: startLine, EndLine: endLine,
				}, decl)
			case *ast.GenDecl:
				if decl.Tok == token.IMPORT {
					continue
				}
				for _, spec := range decl.Specs {
					start, end := spec.Pos(), spec.End()
					doc := specDoc(spec)
					if len(decl.Specs) == 1 {
						start, end = decl.Pos(), decl.End()
						if decl.Doc != nil {
							start = decl.Doc.Pos()
						}
					} else if doc != nil {
						start = doc.Pos()
					}
					startLine, endLine := lines(start, end)
					name := specName(spec)
					id := pkg.PkgPath + "." + name
					if name == "_" {
						id = fmt.Sprintf("%s#%s:%d", id, filename, startLine)
					}
					add(declaration{
						ID: id, Name: name, Kind: strings.ToLower(decl.Tok.String()), File: filename,
						StartLine: startLine, EndLine: endLine,
					}, spec)
				}
			}
		}
	}
	return output
}

func funcID(pkgPath string, decl *ast.FuncDecl) (string, string) {
	if decl.Recv == nil || len(decl.Recv.List) == 0 {
		return pkgPath + "." + decl.Name.Name, "func"
	}
	return pkgPath + "." + receiverTypeName(decl.Recv.List[0].Type) + "." + decl.Name.Name, "method"
}

func funcName(decl *ast.FuncDecl) string {
	if decl.Recv == nil || len(decl.Recv.List) == 0 {
		return decl.Name.Name
	}
	return receiverTypeName(decl.Recv.List[0].Type) + "." + decl.Name.Name
}

func receiverTypeName(expr ast.Expr) string {
	switch expr := expr.(type) {
	case *ast.StarExpr:
		return receiverTypeName(expr.X)
	case *ast.IndexExpr:
		return receiverTypeName(expr.X)
	case *ast.IndexListExpr:
		return receiverTypeName(expr.X)
	case *ast.ParenExpr:
		return receiverTypeName(expr.X)
	case *ast.Ident:
		return expr.Name
	default:
		return "?"
	}
}

func specDoc(spec ast.Spec) *ast.CommentGroup {
	switch spec := spec.(type) {
	case *ast.TypeSpec:
		return spec.Doc
	case *ast.ValueSpec:
		return spec.Doc
	default:
		return nil
	}
}

// specName is the first name of a spec. Other names of a value spec (`var a, b = 1, 2`) map to
// the same declaration, see objectID.
func specName(spec ast.Spec) string {
	switch spec := spec.(type) {
	case *ast.TypeSpec:
		return spec.Name.Name
	case *ast.ValueSpec:
		if len(spec.Names) > 0 {
			return spec.Names[0].Name
		}
	}
	return "_"
}

func collectUses(pkg *packages.Package, node ast.Node, self string, aliases *aliases) []string {
	uses := map[string]bool{}
	ast.Inspect(node, func(node ast.Node) bool {
		ident, ok := node.(*ast.Ident)
		if !ok {
			return true
		}
		obj := pkg.TypesInfo.Uses[ident]
		if obj == nil {
			return true
		}
		if id := objectID(pkg.Fset, obj, aliases); id != "" && id != self {
			uses[id] = true
		}
		return true
	})
	result := make([]string, 0, len(uses))
	for id := range uses {
		result = append(result, id)
	}
	sort.Strings(result)
	return result
}

// objectID returns the declaration ID of an object, or "" for objects that are not top-level
// declarations (locals, parameters, and objects of the universe scope).
func objectID(fset *token.FileSet, obj types.Object, aliases *aliases) string {
	if obj.Pkg() == nil {
		return ""
	}
	switch obj := obj.(type) {
	case *types.Var:
		if obj.IsField() {
			return aliases.fieldOwners[positionKey(fset, obj.Pos())]
		}
	case *types.Func:
		signature, ok := obj.Type().(*types.Signature)
		if ok && signature.Recv() != nil {
			named := baseNamed(signature.Recv().Type())
			if named == nil {
				return ""
			}
			typeName := named.Obj()
			// A method of an interface is a part of the interface declaration.
			if _, isInterface := named.Underlying().(*types.Interface); isInterface {
				return typeName.Pkg().Path() + "." + typeName.Name()
			}
			return typeName.Pkg().Path() + "." + typeName.Name() + "." + obj.Name()
		}
	}
	if obj.Parent() != obj.Pkg().Scope() {
		return ""
	}
	if first, ok := aliases.valueSpecFirstNames[positionKey(fset, obj.Pos())]; ok {
		return obj.Pkg().Path() + "." + first
	}
	return obj.Pkg().Path() + "." + obj.Name()
}

func baseNamed(t types.Type) *types.Named {
	if pointer, ok := t.(*types.Pointer); ok {
		t = pointer.Elem()
	}
	named, ok := types.Unalias(t).(*types.Named)
	if !ok {
		return nil
	}
	return named.Origin()
}
