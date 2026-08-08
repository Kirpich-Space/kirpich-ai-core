import bpy
from pathlib import Path

def main():
    # Remove all existing objects to start with a clean scene
    bpy.ops.object.select_all(action='SELECT')
    bpy.ops.object.delete()

    # Create a cube
    bpy.ops.mesh.primitive_cube_add(size=1, location=(0, 0, 0))
    cube = bpy.context.active_object

    # Create a red material using nodes
    mat = bpy.data.materials.new(name="RedMaterial")
    mat.use_nodes = True
    nodes = mat.node_tree.nodes
    # Find the Principled BSDF node
    bsdf = next((n for n in nodes if n.type == 'BSDF_PRINCIPLED'), None)
    if bsdf:
        # Set Base Color to Red (R=1, G=0, B=0, A=1)
        bsdf.inputs['Base Color'].default_value = (1.0, 0.0, 0.0, 1.0)
    
    # Assign the material to the cube
    if cube.data.materials:
        cube.data.materials[0] = mat
    else:
        cube.data.materials.append(mat)

    # Add a camera so we can render the scene
    bpy.ops.object.camera_add(location=(3, -3, 2), rotation=(1.1, 0, 0.785))
    cam = bpy.context.active_object
    bpy.context.scene.camera = cam

    # Add a light source (Sun) so the cube is visible
    bpy.ops.object.light_add(type='SUN', location=(5, 5, 5))

    # Configure Render Settings
    scene = bpy.context.scene
    scene.render.resolution_x = 320
    scene.render.resolution_y = 240
    
    output_path_str = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Gemma27/task2.png"
    
    # Ensure the output directory exists
    try:
        output_dir = Path(output_path_str).parent
        output_dir.mkdir(parents=True, exist_ok=True)
    except Exception:
        pass

    scene.render.filepath = output_path_str

    # Perform the render and save the image
    bpy.ops.render.render(write_still=True)

if __name__ == "__main__":
    main()