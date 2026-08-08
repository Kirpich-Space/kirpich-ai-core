

"""
import bpy
from pathlib import Path

# Clean slate
for obj in bpy.data.objects:
    if obj.type in {'MESH', 'CAMERA'}:
        bpy.data.objects.remove(obj, do_linked=False)

bpy.context.collection.objects.link(bpy.ops.mesh.primitive_cube_add(location=(0, 0, 0)).object)
cube_obj = bpy.context.object.name

# Camera
for obj in bpy.data.objects:
    if obj.type == 'CAMERA':
        bpy.data.objects.remove(obj)
bpy.ops.camera.add_camera()
cam = bpy.context.object.name
cam_loc = (0, 0, 5)
cam_obj = bpy.data.cameras[cam]
cam_obj.location[:2] = cam_loc[:2]

# Material
material_names = set()
for obj in bpy.data.objects:
    for matname in [m.name for m in obj.material_slots]:
        material_names.add(matname)
if material_names:
    for name in list(material_names):
        del bpy.data.materials[name]

mat = bpy.data.materials.new(name="red_mat")
tree = mat.node_tree
out = tree.nodes.get("Output")
if not out:
    out = tree.nodes.new('ShaderNodeOutput')
    tree.nodes.append(out)
const_node = tree.nodes.new('ShaderNodeConstant')
tree.nodes.append(const_node)
base_socket = next(s for s in out.inputs if s.name == 'Base Color')
tree.links.new(const_node.outputs[0], base_socket)

cube_obj.data.materials.append(mat)

# Lamp
for obj in bpy.data.objects:
    if obj.type == 'LIGHT':
        bpy.data.objects.remove(obj)
lamp = bpy.ops.object.lamp_add().object
lamp.location[:2] = (1.5, 2.0)
lamp.data.energy = 50
lamp.data.size = 3

# Render settings
bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240
out_dir = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.7_en_r2"
Path(out_dir).mkdir(parents=True, exist_ok=True)
bpy.context.scene.render.filepath = Path(out_dir) / "task2.png"

# Report dict
report = {"cam": cam, "lamp": lamp.name}
Path(r"C:\Users\KIRPIC~1\AppData\Local\Temp").joinpath("task_report.json").write_text(str(report))

bpy.ops.render.render(write_still=True)
"""