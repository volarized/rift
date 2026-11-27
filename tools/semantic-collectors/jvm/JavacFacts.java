import com.sun.source.tree.ClassTree;
import com.sun.source.tree.CompilationUnitTree;
import com.sun.source.tree.IdentifierTree;
import com.sun.source.tree.ImportTree;
import com.sun.source.tree.LambdaExpressionTree;
import com.sun.source.tree.MemberReferenceTree;
import com.sun.source.tree.MemberSelectTree;
import com.sun.source.tree.MethodInvocationTree;
import com.sun.source.tree.MethodTree;
import com.sun.source.tree.NewClassTree;
import com.sun.source.tree.Tree;
import com.sun.source.tree.VariableTree;
import com.sun.source.util.JavacTask;
import com.sun.source.util.TreePath;
import com.sun.source.util.TreePathScanner;
import com.sun.source.util.Trees;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import javax.lang.model.element.Element;
import javax.lang.model.element.ElementKind;
import javax.lang.model.element.VariableElement;
import javax.lang.model.type.TypeKind;
import javax.lang.model.util.Elements;
import javax.tools.Diagnostic;
import javax.tools.StandardLocation;
import javax.tools.ToolProvider;

public final class JavacFacts {
    private JavacFacts() {}

    static void collect(Facts facts, List<Path> paths, List<Path> classpath, Path output,
            String patchModule) throws IOException {
        var compiler = ToolProvider.getSystemJavaCompiler();
        try (var files = compiler.getStandardFileManager(null, null, StandardCharsets.UTF_8)) {
            // A separate source path prevents javac from reading unadmitted neighboring files.
            files.setLocationFromPaths(StandardLocation.SOURCE_PATH, List.of());
            files.setLocationFromPaths(StandardLocation.CLASS_OUTPUT, List.of(output));
            files.setLocationFromPaths(StandardLocation.CLASS_PATH, classpath);
            var options = new ArrayList<>(List.of("-proc:none", "-implicit:none"));
            if (patchModule != null) {
                options.addAll(List.of("--patch-module", patchModule));
            }
            var task = (JavacTask) compiler.getTask(null, files, diagnostic -> {
                if (diagnostic.getKind() == Diagnostic.Kind.ERROR) {
                    facts.partial = true;
                }
            }, options, null, files.getJavaFileObjectsFromPaths(paths));
            var units = new ArrayList<CompilationUnitTree>();
            task.parse().forEach(units::add);
            task.analyze();
            var trees = Trees.instance(task);
            for (var unit : units) {
                if (Integer.valueOf(0).equals(facts.source(Path.of(unit.getSourceFile().toUri())))) {
                    new Scanner(facts, trees, task.getElements(), unit, unit.getSourceFile().getCharContent(true))
                        .scan(unit, null);
                    break;
                }
            }
        } catch (StackOverflowError error) {
            facts.partial = true;
        }
    }

    private static final class Scanner extends TreePathScanner<Void, Facts.Range> {
        private final Facts facts;
        private final Trees trees;
        private final Elements elements;
        private final CompilationUnitTree unit;
        private final String text;

        Scanner(Facts facts, Trees trees, Elements elements, CompilationUnitTree unit, CharSequence text) {
            this.facts = facts;
            this.trees = trees;
            this.elements = elements;
            this.unit = unit;
            this.text = text.toString();
        }

        private Facts.Range range(CompilationUnitTree source, Tree tree) {
            var positions = trees.getSourcePositions();
            long start = positions.getStartPosition(source, tree);
            long end = positions.getEndPosition(source, tree);
            return start < 0 || end <= start || end > Integer.MAX_VALUE
                ? null : new Facts.Range((int) start, (int) end);
        }

        private Facts.Anchor target(TreePath path) {
            return target(trees.getElement(path));
        }

        private Facts.Anchor target(Element element) {
            if (element == null) {
                return null;
            }
            // Javac can bind a variable even when its declared type is missing.
            boolean missingType = element.asType().getKind() == TypeKind.ERROR;
            if (missingType && !(element instanceof VariableElement)) {
                return null;
            }
            TreePath target = trees.getPath(element);
            if (target == null || (missingType && !(target.getLeaf() instanceof VariableTree))) {
                return null;
            }
            var source = target.getCompilationUnit();
            Integer number = facts.source(Path.of(source.getSourceFile().toUri()));
            var coordinates = range(source, target.getLeaf());
            return number == null || coordinates == null ? null : new Facts.Anchor(number, coordinates);
        }

        private void record(Tree tree, Facts.Range owner, TreePath targetPath) {
            record(range(unit, tree), owner, targetPath);
        }

        private void record(Facts.Range occurrence, Facts.Range owner, TreePath targetPath) {
            if (occurrence != null) {
                facts.add(occurrence, owner, target(targetPath));
            }
        }

        private Facts.Range memberRange(MemberSelectTree tree) {
            var coordinates = range(unit, tree);
            if (coordinates == null) {
                return null;
            }
            String name = tree.getIdentifier().toString();
            int start = coordinates.end() - name.length();
            return start >= coordinates.start() && text.startsWith(name, start)
                ? new Facts.Range(start, coordinates.end()) : coordinates;
        }

        @Override
        public Void visitCompilationUnit(CompilationUnitTree tree, Facts.Range owner) {
            scan(tree.getPackageAnnotations(), owner);
            scan(tree.getImports(), owner);
            scan(tree.getTypeDecls(), owner);
            scan(tree.getModule(), owner);
            return null;
        }

        @Override
        public Void visitClass(ClassTree tree, Facts.Range owner) {
            return super.visitClass(tree, range(unit, tree));
        }

        @Override
        public Void visitMethod(MethodTree tree, Facts.Range owner) {
            return super.visitMethod(tree, range(unit, tree));
        }

        @Override
        public Void visitLambdaExpression(LambdaExpressionTree tree, Facts.Range owner) {
            return super.visitLambdaExpression(tree, range(unit, tree));
        }

        @Override
        public Void visitImport(ImportTree tree, Facts.Range owner) {
            if (facts.category == Facts.Category.IMPORTS) {
                Tree name = tree.getQualifiedIdentifier();
                var path = new TreePath(getCurrentPath(), name);
                if (tree.isStatic() && name instanceof MemberSelectTree member
                        && member.getIdentifier().contentEquals("*")) {
                    Tree qualifier = member.getExpression();
                    var occurrence = qualifier instanceof MemberSelectTree selection
                        ? memberRange(selection) : range(unit, qualifier);
                    record(occurrence, null, new TreePath(path, qualifier));
                } else {
                    record(name, null, path);
                }
            }
            return null;
        }

        @Override
        public Void visitMethodInvocation(MethodInvocationTree tree, Facts.Range owner) {
            if (facts.category == Facts.Category.CALLS) {
                record(tree, owner, new TreePath(getCurrentPath(), tree.getMethodSelect()));
            }
            return super.visitMethodInvocation(tree, owner);
        }

        private boolean recordEnumConstructor(NewClassTree initializer, Facts.Range owner) {
            var path = getCurrentPath();
            var parent = path.getParentPath();
            if (!(parent.getLeaf() instanceof VariableTree tree) || tree.getInitializer() != initializer) {
                return false;
            }
            var constant = trees.getElement(parent);
            if (constant == null || constant.getKind() != ElementKind.ENUM_CONSTANT) {
                return false;
            }
            var enumType = constant.getEnclosingElement();
            var constructor = trees.getElement(path);
            if (constructor == null || constructor.getKind() != ElementKind.CONSTRUCTOR
                    || !enumType.equals(constructor.getEnclosingElement())) {
                return false;
            }

            // Javac places the initializer's type tree at the authored constant name.
            long start = trees.getSourcePositions().getStartPosition(unit, initializer.getIdentifier());
            String name = tree.getName().toString();
            if (start < 0 || start > text.length() - name.length() || !text.startsWith(name, (int) start)) {
                return false;
            }

            var binding = target(constructor);
            if (binding == null && elements.getOrigin(constructor) == Elements.Origin.MANDATED) {
                binding = target(enumType);
            }
            facts.add(new Facts.Range((int) start, (int) start + name.length()), owner, binding);
            return true;
        }

        @Override
        public Void visitNewClass(NewClassTree tree, Facts.Range owner) {
            boolean enumConstructor = facts.category != Facts.Category.IMPORTS && recordEnumConstructor(tree, owner);
            if (facts.category == Facts.Category.CALLS && !enumConstructor) {
                record(tree, owner, getCurrentPath());
            }
            return super.visitNewClass(tree, owner);
        }

        @Override
        public Void visitIdentifier(IdentifierTree tree, Facts.Range owner) {
            if (facts.category == Facts.Category.REFERENCES) {
                record(tree, owner, getCurrentPath());
            }
            return super.visitIdentifier(tree, owner);
        }

        @Override
        public Void visitMemberSelect(MemberSelectTree tree, Facts.Range owner) {
            if (facts.category == Facts.Category.REFERENCES) {
                record(memberRange(tree), owner, getCurrentPath());
            }
            return super.visitMemberSelect(tree, owner);
        }

        @Override
        public Void visitMemberReference(MemberReferenceTree tree, Facts.Range owner) {
            if (facts.category == Facts.Category.REFERENCES) {
                record(tree, owner, getCurrentPath());
            }
            return super.visitMemberReference(tree, owner);
        }
    }
}
